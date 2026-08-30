//! Coroutine runtime adaptation from Luau handlers to core AgentTool.

use super::bindings::{CapabilityBindings, CapabilityError, CapabilityFuture, CapabilityRequest};
#[cfg(test)]
pub(super) use super::bindings::{CapabilityResponse, LuauCapability};
use super::specs::{
    validate_limits, validate_spec, HandlerLimits, ToolHandlerInitError, ToolHandlerSpec,
};
use crate::bundle_runtime::BundleRuntime;
use mlua::thread::ThreadStatus;
use mlua::{Function, Lua, LuaOptions, StdLib, Table, Thread, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tea_core::effect::RunProvenance;
use tea_core::error::ToolError;
#[cfg(test)]
pub(super) use tea_core::scheduler::CancellationToken;
use tea_core::scheduler::CancellationWait;
use tea_core::state::{SerializedJson, ToolCallId, Usage};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolExecutionMode, ToolFuture, ToolUpdate,
    ToolUpdateSink,
};
use tea_protocol::{JsonNumber, JsonValue};

const HANDLER_CHUNK_NAME: &str = "tea-tool-handler.luau";

/// Largest presentation payload one handler yield may hand to the host.
///
/// Activity text is transient terminal presentation, never durable state or
/// model context, so it is bounded independently of result/detail limits.
pub(super) const MAX_ACTIVITY_BYTES: usize = 8 * 1024;

/// Where one handler's executable Luau comes from.
#[derive(Clone)]
enum HandlerProgram {
    /// A self-contained chunk evaluated with no module loader at all.
    Source(Arc<str>),
    /// A module inside the extension's own closed bundle.
    ///
    /// The invocation VM installs that bundle's `require`, so a checked-in
    /// extension can factor shared logic into sibling modules instead of
    /// duplicating it. The loader resolves only bundle-local `./` and `../`
    /// imports, so this adds no filesystem, package, or host authority.
    Module {
        runtime: Arc<BundleRuntime>,
        path: String,
    },
}

/// A sandboxed coroutine-backed implementation of [`AgentTool`].
pub struct LuaToolHandler {
    program: HandlerProgram,
    spec: ToolHandlerSpec,
    capabilities: CapabilityBindings,
    limits: HandlerLimits,
}

impl fmt::Debug for LuaToolHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LuaToolHandler")
            .field("name", &self.spec.name)
            .field("capability", &self.spec.capability)
            .field("limits", &self.limits)
            .finish()
    }
}

impl LuaToolHandler {
    /// Load a handler with conservative default VM limits.
    ///
    /// The source must evaluate to a function accepting one call table. A
    /// capability request is yielded as:
    ///
    /// ```text
    /// { kind = "capability", capability = "world", method = "mcp.call", arguments = {} }
    /// ```
    ///
    /// `arguments_json` remains accepted for existing extensions, but checked-in
    /// builtins should use the structured `arguments` table. The adapter converts
    /// it to protocol JSON before the host capability validates it.
    ///
    /// The function returns either a content string or a result table with a
    /// required `content` field and optional `details_json`, `terminate`, and
    /// `is_error` fields.
    pub fn new(
        source: impl Into<String>,
        spec: ToolHandlerSpec,
        capabilities: CapabilityBindings,
    ) -> Result<Self, ToolHandlerInitError> {
        Self::new_with_limits(source, spec, capabilities, HandlerLimits::default())
    }

    /// Load a handler with finite, host-selected VM limits.
    pub fn new_with_limits(
        source: impl Into<String>,
        spec: ToolHandlerSpec,
        capabilities: CapabilityBindings,
        limits: HandlerLimits,
    ) -> Result<Self, ToolHandlerInitError> {
        let source: Arc<str> = source.into().into();
        Self::load(
            HandlerProgram::Source(Arc::clone(&source)),
            source.len(),
            spec,
            capabilities,
            limits,
        )
    }

    /// Load a handler from one module of its own closed extension bundle.
    ///
    /// The module may `require` sibling modules from the same bundle and
    /// nothing else. This exists so an extension can share one definition of
    /// its semantics between an executable handler and its policy VM.
    pub fn new_bundle_module(
        runtime: Arc<BundleRuntime>,
        path: impl Into<String>,
        spec: ToolHandlerSpec,
        capabilities: CapabilityBindings,
        limits: HandlerLimits,
    ) -> Result<Self, ToolHandlerInitError> {
        let path = path.into();
        // The declared entry module must exist before anything else is
        // validated, so a typo is a load-time contract error.
        let source_bytes = crate::bundle::ModulePath::new(&path)
            .ok()
            .and_then(|module| runtime.bundle().module(&module).map(str::len))
            .ok_or_else(|| ToolHandlerInitError::Contract {
                message: format!("handler module {path:?} is not part of the extension bundle"),
            })?;
        Self::load(
            HandlerProgram::Module { runtime, path },
            source_bytes,
            spec,
            capabilities,
            limits,
        )
    }

    fn load(
        program: HandlerProgram,
        source_bytes: usize,
        spec: ToolHandlerSpec,
        capabilities: CapabilityBindings,
        limits: HandlerLimits,
    ) -> Result<Self, ToolHandlerInitError> {
        validate_limits(limits)?;
        validate_spec(&spec)?;
        if source_bytes > limits.max_source_bytes {
            return Err(ToolHandlerInitError::SourceTooLarge {
                actual: source_bytes,
                limit: limits.max_source_bytes,
            });
        }
        if capabilities.get(&spec.capability).is_none() {
            return Err(ToolHandlerInitError::UnboundCapability {
                capability: spec.capability.clone(),
            });
        }

        // Compile once at construction so a malformed handler never reaches
        // the registry. Each actual invocation gets a fresh VM below.
        build_runtime(&program, limits)?;

        Ok(Self {
            program,
            spec,
            capabilities,
            limits,
        })
    }

    /// Return the prompt-facing handler metadata.
    pub fn spec(&self) -> &ToolHandlerSpec {
        &self.spec
    }
}

impl AgentTool for LuaToolHandler {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn schema(&self) -> &JsonValue {
        &self.spec.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.spec.execution_mode
    }

    fn requires_exclusive_batch(&self) -> bool {
        self.spec.requires_exclusive_batch
    }

    fn cancellation_settlement_mode(&self) -> tea_core::tool::CancellationSettlementMode {
        self.spec.cancellation_settlement_mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let cancellation_wait = context.cancellation.cancelled();
        Box::pin(LuaToolExecution {
            handler: self,
            call: Some(call),
            context: Some(context),
            updates,
            runtime: None,
            capability_future: None,
            resume_value: None,
            cancellation_wait: Some(cancellation_wait),
            capability_calls: 0,
            observable_yields: 0,
        })
    }
}

struct InvocationRuntime {
    // Keeping the Lua owner alive keeps the Thread's registry reference valid.
    _lua: Lua,
    thread: Thread,
    interrupt_budget: Arc<AtomicUsize>,
    max_interrupt_checks: usize,
}

struct LuaToolExecution<'a> {
    handler: &'a LuaToolHandler,
    call: Option<ToolCall>,
    context: Option<ToolContext>,
    updates: ToolUpdateSink,
    runtime: Option<InvocationRuntime>,
    capability_future: Option<CapabilityFuture>,
    resume_value: Option<JsonValue>,
    cancellation_wait: Option<CancellationWait>,
    capability_calls: usize,
    /// Every externally observable yield, whether it invoked a host capability
    /// or only replaced host presentation. One finite budget keeps a handler
    /// from driving an unbounded host-update loop.
    observable_yields: usize,
}

impl Future for LuaToolExecution<'_> {
    type Output = Result<AgentToolResult, ToolError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let tool_name = this.handler.spec.name.clone();

        loop {
            let cancellation = this
                .context
                .as_ref()
                .expect("tool context remains until execution settles")
                .cancellation
                .clone();
            let cancellation_won = this
                .cancellation_wait
                .as_mut()
                .is_some_and(|wait| Pin::new(wait).poll(context).is_ready());
            if cancellation.is_cancelled() || cancellation_won {
                // Drop a pending host future before publishing cancellation,
                // so settlement never leaves capability work owned by this
                // adapter.
                this.capability_future.take();
                return Poll::Ready(Err(ToolError::Cancelled { tool: tool_name }));
            }

            if let Some(future) = this.capability_future.as_mut() {
                match future.as_mut().poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(response)) => {
                        this.capability_future = None;
                        this.resume_value = Some(response.value);
                        continue;
                    }
                    Poll::Ready(Err(error)) => {
                        this.capability_future = None;
                        return Poll::Ready(Err(map_capability_error(&tool_name, error)));
                    }
                }
            }

            if this.runtime.is_none() {
                let runtime = match build_runtime(&this.handler.program, this.handler.limits) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        return Poll::Ready(Err(ToolError::Execution {
                            tool: tool_name,
                            message: error.to_string(),
                        }));
                    }
                };
                this.runtime = Some(runtime);
            }

            let (yielded_or_returned, status) = {
                let runtime = this
                    .runtime
                    .as_mut()
                    .expect("runtime initialized immediately above");
                runtime
                    .interrupt_budget
                    .store(runtime.max_interrupt_checks, Ordering::Relaxed);

                let resume_argument = if let Some(value) = this.resume_value.take() {
                    match json_to_lua(&runtime._lua, &value) {
                        Ok(value) => value,
                        Err(error) => {
                            return Poll::Ready(Err(ToolError::Execution {
                                tool: tool_name,
                                message: format!(
                                    "capability response could not enter Luau: {error}"
                                ),
                            }));
                        }
                    }
                } else {
                    let call = this.call.as_ref().expect("call remains until completion");
                    match call_to_lua(
                        &runtime._lua,
                        call,
                        &this
                            .context
                            .as_ref()
                            .expect("context remains until execution settles")
                            .provenance,
                    ) {
                        Ok(value) => value,
                        Err(error) => return Poll::Ready(Err(error)),
                    }
                };

                let yielded_or_returned = match runtime.thread.resume::<Value>(resume_argument) {
                    Ok(value) => value,
                    Err(error) => {
                        return Poll::Ready(Err(ToolError::Execution {
                            tool: tool_name,
                            message: error.to_string(),
                        }));
                    }
                };
                let status = runtime.thread.status();
                (yielded_or_returned, status)
            };

            match status {
                ThreadStatus::Resumable => {
                    let yielded = match parse_handler_yield(
                        yielded_or_returned,
                        this.call
                            .as_ref()
                            .expect("call remains until completion")
                            .id
                            .clone(),
                        this.call
                            .as_ref()
                            .expect("call remains until completion")
                            .name
                            .clone(),
                        this.context
                            .as_ref()
                            .expect("context remains until execution settles")
                            .provenance
                            .clone(),
                        this.updates.clone(),
                    ) {
                        Ok(yielded) => yielded,
                        Err(message) => {
                            return Poll::Ready(Err(ToolError::Execution {
                                tool: tool_name,
                                message,
                            }));
                        }
                    };
                    let request = match yielded {
                        HandlerYield::Update(update) => {
                            if this.observable_yields == this.handler.limits.max_capability_calls {
                                return Poll::Ready(Err(ToolError::Execution {
                                    tool: tool_name,
                                    message: format!(
                                        "Luau handler exceeded its {} yield limit",
                                        this.handler.limits.max_capability_calls
                                    ),
                                }));
                            }
                            this.observable_yields += 1;
                            // A presentation update invokes no host capability
                            // and carries no authority: publish it and resume
                            // the coroutine immediately.
                            this.updates.emit(update);
                            this.resume_value = Some(JsonValue::Null);
                            continue;
                        }
                        HandlerYield::Capability(request) => request,
                    };
                    if request.capability != this.handler.spec.capability {
                        return Poll::Ready(Err(ToolError::Blocked {
                            tool: tool_name,
                            reason: format!(
                                "Luau requested capability {:?}, but only {:?} is bound for this tool",
                                request.capability, this.handler.spec.capability
                            ),
                        }));
                    }
                    let Some(capability) = this.handler.capabilities.get(&request.capability)
                    else {
                        return Poll::Ready(Err(ToolError::Blocked {
                            tool: tool_name,
                            reason: format!(
                                "capability {:?} is not explicitly bound",
                                request.capability
                            ),
                        }));
                    };
                    if this.capability_calls == this.handler.limits.max_capability_calls {
                        return Poll::Ready(Err(ToolError::Execution {
                            tool: tool_name,
                            message: format!(
                                "Luau handler exceeded its {} capability-call limit",
                                this.handler.limits.max_capability_calls
                            ),
                        }));
                    }
                    if this.observable_yields == this.handler.limits.max_capability_calls {
                        return Poll::Ready(Err(ToolError::Execution {
                            tool: tool_name,
                            message: format!(
                                "Luau handler exceeded its {} yield limit",
                                this.handler.limits.max_capability_calls
                            ),
                        }));
                    }
                    this.capability_calls += 1;
                    this.observable_yields += 1;
                    this.capability_future = Some(capability.invoke(request, cancellation));
                }
                ThreadStatus::Finished => {
                    let call_id = this
                        .call
                        .as_ref()
                        .expect("call remains until completion")
                        .id
                        .clone();
                    let result = match parse_tool_result(yielded_or_returned) {
                        Ok(result) => result,
                        Err(message) => {
                            return Poll::Ready(Err(ToolError::Execution {
                                tool: tool_name,
                                message,
                            }));
                        }
                    };
                    return Poll::Ready(Ok(AgentToolResult {
                        tool_call_id: call_id,
                        content: result.content,
                        details: result.details,
                        usage: result.usage,
                        added_tool_names: Vec::new(),
                        terminate: result.terminate,
                        is_error: result.is_error,
                        failure: None,
                    }));
                }
                status => {
                    return Poll::Ready(Err(ToolError::Execution {
                        tool: tool_name,
                        message: format!("handler coroutine reached unexpected status {status:?}"),
                    }));
                }
            }
        }
    }
}

struct LuaToolResult {
    content: String,
    details: Option<SerializedJson>,
    usage: Option<Usage>,
    terminate: bool,
    is_error: bool,
}

fn build_runtime(
    program: &HandlerProgram,
    limits: HandlerLimits,
) -> Result<InvocationRuntime, ToolHandlerInitError> {
    let lua = Lua::new_with(
        StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING | StdLib::UTF8 | StdLib::MATH,
        LuaOptions::new(),
    )
    .map_err(runtime_error)?;
    lua.set_memory_limit(limits.max_memory_bytes)
        .map_err(runtime_error)?;
    lua.enable_jit(true);
    if let HandlerProgram::Module { runtime, .. } = program {
        // The closed bundle loader must be installed before sandboxing makes
        // globals read-only.
        runtime
            .install(&lua)
            .map_err(|error| ToolHandlerInitError::Runtime {
                message: error.to_string(),
            })?;
    }
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
                "Luau handler interrupt budget exhausted".to_owned(),
            ));
        }
        Ok(mlua::VmState::Continue)
    });

    let function: Function = match program {
        HandlerProgram::Source(source) => lua
            .load(&**source)
            .set_name(HANDLER_CHUNK_NAME)
            .eval()
            .map_err(runtime_error)?,
        HandlerProgram::Module { runtime, path } => {
            match runtime.eval_module(&lua, path).map_err(|error| {
                ToolHandlerInitError::Runtime {
                    message: error.to_string(),
                }
            })? {
                Value::Function(function) => function,
                other => {
                    return Err(ToolHandlerInitError::Contract {
                        message: format!(
                            "handler module {path:?} returned {}, expected a function",
                            other.type_name()
                        ),
                    });
                }
            }
        }
    };
    let thread = lua.create_thread(function).map_err(runtime_error)?;
    Ok(InvocationRuntime {
        _lua: lua,
        thread,
        interrupt_budget,
        max_interrupt_checks: limits.max_interrupt_checks,
    })
}

fn call_to_lua(lua: &Lua, call: &ToolCall, provenance: &RunProvenance) -> Result<Value, ToolError> {
    let arguments =
        JsonValue::parse(call.arguments.as_str()).map_err(|error| ToolError::InvalidArguments {
            tool: call.name.clone(),
            message: error.to_string(),
        })?;
    let table = lua.create_table().map_err(|error| ToolError::Execution {
        tool: call.name.clone(),
        message: error.to_string(),
    })?;
    table
        .set("id", call.id.as_str())
        .and_then(|_| table.set("name", call.name.as_str()))
        .and_then(|_| table.set("arguments_json", call.arguments.as_str()))
        .and_then(|_| json_to_lua(lua, &arguments).and_then(|value| table.set("arguments", value)))
        .map_err(|error| ToolError::Execution {
            tool: call.name.clone(),
            message: error.to_string(),
        })?;
    let provenance = provenance_to_lua(lua, provenance).map_err(|error| ToolError::Execution {
        tool: call.name.clone(),
        message: error.to_string(),
    })?;
    table
        .set("provenance", provenance)
        .map_err(|error| ToolError::Execution {
            tool: call.name.clone(),
            message: error.to_string(),
        })?;
    Ok(Value::Table(table))
}

fn provenance_to_lua(lua: &Lua, provenance: &RunProvenance) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (name, value) in [
        ("session_id", provenance.session_id.as_deref()),
        ("lane_id", provenance.lane_id.as_deref()),
        ("operation_id", provenance.operation_id.as_deref()),
        ("epoch_id", provenance.epoch_id.as_deref()),
        ("source_leaf_id", provenance.source_leaf_id.as_deref()),
        ("core_run_id", provenance.core_run_id.as_deref()),
        (
            "harness_revision_id",
            provenance.harness_revision_id.as_deref(),
        ),
        (
            "harness_snapshot_id",
            provenance.harness_snapshot_id.as_deref(),
        ),
        (
            "model_harness_profile_id",
            provenance.model_harness_profile_id.as_deref(),
        ),
        (
            "provider_surface_digest",
            provenance.provider_surface_digest.as_deref(),
        ),
        ("experiment_id", provenance.experiment_id.as_deref()),
    ] {
        if let Some(value) = value {
            table.set(name, value)?;
        }
    }
    Ok(table)
}

/// What a suspended handler asked the host to do before it resumes.
enum HandlerYield {
    /// Invoke the one capability explicitly bound to this tool.
    Capability(CapabilityRequest),
    /// Replace transient host presentation. This performs no host operation,
    /// produces no model-visible result, and never becomes durable state.
    Update(ToolUpdate),
}

fn parse_handler_yield(
    value: Value,
    call_id: ToolCallId,
    tool_name: String,
    provenance: RunProvenance,
    updates: ToolUpdateSink,
) -> Result<HandlerYield, String> {
    let table = match value {
        Value::Table(table) => table,
        other => {
            return Err(format!(
                "handler yielded {}, expected a capability or update table",
                other.type_name()
            ));
        }
    };
    let kind = required_string(&table, "kind")?;
    match kind.as_str() {
        "capability" => parse_capability_request(table, call_id, tool_name, provenance, updates)
            .map(HandlerYield::Capability),
        "update" => parse_update(table).map(HandlerYield::Update),
        _ => Err(format!("handler yielded unsupported request kind {kind:?}")),
    }
}

/// Read one presentation-only update yield.
///
/// The shape is deliberately closed: an unknown field is a contract error
/// rather than a silently ignored request for host behavior that does not
/// exist.
fn parse_update(table: Table) -> Result<ToolUpdate, String> {
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|error| format!("update yield table is invalid: {error}"))?;
        let Value::String(key) = key else {
            return Err("update yield fields must be named by strings".into());
        };
        let key = key
            .to_str()
            .map_err(|error| format!("update yield field is not UTF-8: {error}"))?
            .to_owned();
        if !matches!(key.as_str(), "kind" | "content" | "activity") {
            return Err(format!("update yield has unknown field {key:?}"));
        }
    }
    let content = optional_exact_string(&table, "content")?;
    let activity = optional_exact_string(&table, "activity")?;
    if content.is_none() && activity.is_none() {
        return Err("update yield requires content, activity, or both".into());
    }
    if activity
        .as_ref()
        .is_some_and(|activity| activity.len() > MAX_ACTIVITY_BYTES)
    {
        return Err(format!(
            "update activity exceeds {MAX_ACTIVITY_BYTES} bytes"
        ));
    }
    Ok(ToolUpdate {
        content: content.unwrap_or_default(),
        details: None,
        activity,
    })
}

fn parse_capability_request(
    table: Table,
    call_id: ToolCallId,
    tool_name: String,
    provenance: RunProvenance,
    updates: ToolUpdateSink,
) -> Result<CapabilityRequest, String> {
    let capability = required_string(&table, "capability")?;
    let method = required_string(&table, "method")?;
    let has_structured_arguments = table
        .contains_key("arguments")
        .map_err(|error| format!("capability arguments lookup failed: {error}"))?;
    let has_json_arguments = table
        .contains_key("arguments_json")
        .map_err(|error| format!("capability arguments lookup failed: {error}"))?;
    if has_structured_arguments == has_json_arguments {
        return Err(
            "capability request must contain exactly one of arguments or arguments_json".into(),
        );
    }
    let arguments = if has_structured_arguments {
        let arguments = table
            .get::<Value>("arguments")
            .map_err(|error| format!("capability arguments is invalid: {error}"))?;
        lua_to_json(arguments)?
    } else {
        let arguments_json = required_string(&table, "arguments_json")?;
        JsonValue::parse(&arguments_json)
            .map_err(|error| format!("capability arguments_json is invalid: {error}"))?
    };
    Ok(CapabilityRequest {
        call_id,
        tool_name,
        provenance,
        capability,
        method,
        arguments,
        updates,
    })
}

/// Convert a sandboxed Luau value into the stable protocol JSON value before
/// it crosses the host capability boundary. Tables are either string-keyed
/// objects or contiguous one-indexed arrays; mixed/sparse tables are rejected
/// instead of gaining a host-specific interpretation.
fn lua_to_json(value: Value) -> Result<JsonValue, String> {
    match value {
        Value::Nil => Ok(JsonValue::Null),
        Value::Boolean(value) => Ok(JsonValue::Bool(value)),
        Value::Integer(value) => Ok(JsonValue::Number(JsonNumber::Signed(value))),
        Value::Number(value) if value.is_finite() => {
            Ok(JsonValue::Number(JsonNumber::Float(value)))
        }
        Value::Number(_) => Err("capability arguments cannot contain a non-finite number".into()),
        Value::String(value) => value
            .to_str()
            .map(|value| JsonValue::String(value.to_owned()))
            .map_err(|error| format!("capability argument string is not UTF-8: {error}")),
        Value::Table(table) => {
            let mut object = BTreeMap::new();
            let mut indexed = BTreeMap::new();
            for pair in table.pairs::<Value, Value>() {
                let (key, value) =
                    pair.map_err(|error| format!("capability argument table is invalid: {error}"))?;
                match key {
                    Value::String(key) => {
                        let key = key
                            .to_str()
                            .map_err(|error| format!("capability argument key is not UTF-8: {error}"))?
                            .to_owned();
                        object.insert(key, lua_to_json(value)?);
                    }
                    Value::Integer(index) if index > 0 => {
                        indexed.insert(index as usize, lua_to_json(value)?);
                    }
                    _ => return Err("capability argument tables require string keys or positive integer array indexes".into()),
                }
            }
            if !object.is_empty() && !indexed.is_empty() {
                return Err(
                    "capability argument table cannot mix object fields and array indexes".into(),
                );
            }
            if !indexed.is_empty() {
                let length = indexed.len();
                let mut values = Vec::with_capacity(length);
                for index in 1..=length {
                    values.push(indexed.remove(&index).ok_or_else(|| {
                        "capability argument array indexes must be contiguous from 1".to_owned()
                    })?);
                }
                Ok(JsonValue::Array(values))
            } else {
                Ok(JsonValue::Object(object))
            }
        }
        other => Err(format!(
            "capability arguments cannot contain {}",
            other.type_name()
        )),
    }
}

fn parse_tool_result(value: Value) -> Result<LuaToolResult, String> {
    let table = match value {
        Value::String(value) => {
            return Ok(LuaToolResult {
                content: value
                    .to_str()
                    .map_err(|error| format!("handler result is not UTF-8: {error}"))?
                    .to_owned(),
                details: None,
                usage: None,
                terminate: false,
                is_error: false,
            });
        }
        Value::Table(table) => table,
        other => {
            return Err(format!(
                "handler returned {}, expected string or result table",
                other.type_name()
            ));
        }
    };
    let content = required_string(&table, "content")?;
    let details = table
        .get::<Option<String>>("details_json")
        .map_err(|error| format!("result details_json must be a string: {error}"))?
        .map(|value| {
            JsonValue::parse(&value)
                .map_err(|error| format!("result details_json is invalid: {error}"))
                .map(|_| SerializedJson::new(value))
        })
        .transpose()?;
    let terminate = table
        .get::<Option<bool>>("terminate")
        .map_err(|error| format!("result terminate must be boolean: {error}"))?
        .unwrap_or(false);
    let is_error = table
        .get::<Option<bool>>("is_error")
        .map_err(|error| format!("result is_error must be boolean: {error}"))?
        .unwrap_or(false);
    Ok(LuaToolResult {
        content,
        details,
        usage: None,
        terminate,
        is_error,
    })
}

/// Read an absent-or-string field without Lua's implicit number coercion, so a
/// handler bug becomes a deterministic contract error instead of presentation
/// text the extension never wrote.
fn optional_exact_string(table: &Table, field: &str) -> Result<Option<String>, String> {
    match table
        .get::<Value>(field)
        .map_err(|error| format!("update {field} is invalid: {error}"))?
    {
        Value::Nil => Ok(None),
        Value::String(value) => value
            .to_str()
            .map(|value| Some(value.to_owned()))
            .map_err(|error| format!("update {field} is not UTF-8: {error}")),
        other => Err(format!(
            "update {field} must be a string, not {}",
            other.type_name()
        )),
    }
}

fn required_string(table: &Table, field: &str) -> Result<String, String> {
    table.get::<String>(field).map_err(|error| {
        format!("handler field {field:?} is required and must be a string: {error}")
    })
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

fn map_capability_error(tool: &str, error: CapabilityError) -> ToolError {
    match error {
        CapabilityError::Cancelled => ToolError::Cancelled {
            tool: tool.to_owned(),
        },
        CapabilityError::NotBound { capability } => ToolError::Blocked {
            tool: tool.to_owned(),
            reason: format!("capability {capability:?} is not explicitly bound"),
        },
        CapabilityError::MethodDenied { capability, method } => ToolError::Blocked {
            tool: tool.to_owned(),
            reason: format!("capability {capability:?} denied method {method:?}"),
        },
        CapabilityError::InvalidArguments { message } => ToolError::InvalidArguments {
            tool: tool.to_owned(),
            message,
        },
        CapabilityError::Execution { message } => ToolError::Execution {
            tool: tool.to_owned(),
            message,
        },
    }
}

fn runtime_error(error: mlua::Error) -> ToolHandlerInitError {
    ToolHandlerInitError::Runtime {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Waker;
    use tea_core::state::ToolCallId;

    struct EchoCapability {
        calls: Arc<AtomicUsize>,
        response: Result<CapabilityResponse, CapabilityError>,
    }

    impl LuauCapability for EchoCapability {
        fn invoke(
            &self,
            _request: CapabilityRequest,
            _cancellation: CancellationToken,
        ) -> CapabilityFuture {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let response = self.response.clone();
            Box::pin(std::future::ready(response))
        }
    }

    struct PendingCapability {
        drops: Arc<AtomicUsize>,
    }

    struct PendingFuture {
        drops: Arc<AtomicUsize>,
    }

    impl Future for PendingFuture {
        type Output = Result<CapabilityResponse, CapabilityError>;

        fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingFuture {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl LuauCapability for PendingCapability {
        fn invoke(
            &self,
            _request: CapabilityRequest,
            _cancellation: CancellationToken,
        ) -> CapabilityFuture {
            Box::pin(PendingFuture {
                drops: Arc::clone(&self.drops),
            })
        }
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            id: ToolCallId::new("call-1").expect("test call ID is non-empty"),
            name: "echo_tool".to_owned(),
            arguments: SerializedJson::new(r#"{"message":"hello"}"#),
        }
    }

    fn spec() -> ToolHandlerSpec {
        ToolHandlerSpec {
            name: "echo_tool".to_owned(),
            description: "Echo a capability response.".to_owned(),
            schema: JsonValue::object([("type", JsonValue::from("object"))]),
            capability: "world".to_owned(),
            execution_mode: ToolExecutionMode::Sequential,
            requires_exclusive_batch: false,
            cancellation_settlement_mode: tea_core::tool::CancellationSettlementMode::DropFuture,
        }
    }

    fn bindings(capability: Arc<dyn LuauCapability>) -> CapabilityBindings {
        let mut bindings = CapabilityBindings::new();
        bindings
            .insert("world", capability)
            .expect("test binding is unique");
        bindings
    }

    fn noop_waker() -> Waker {
        Waker::noop().clone()
    }

    fn run_to_completion(future: ToolFuture<'_>) -> Result<AgentToolResult, ToolError> {
        let mut future = future;
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        loop {
            match Pin::new(&mut future).poll(&mut context) {
                Poll::Ready(result) => return result,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn coroutine_handler_becomes_an_agent_tool_and_calls_only_bound_capability() {
        let calls = Arc::new(AtomicUsize::new(0));
        let capability = Arc::new(EchoCapability {
            calls: Arc::clone(&calls),
            response: Ok(CapabilityResponse {
                value: JsonValue::from("capability-ok"),
            }),
        });
        let handler = LuaToolHandler::new(
            r#"
                return function(call)
                    local response = coroutine.yield({
                        kind = "capability",
                        capability = "world",
                        method = "echo",
                        arguments_json = call.arguments_json,
                    })
                    return { content = response }
                end
            "#,
            spec(),
            bindings(capability),
        )
        .expect("handler should load");

        let result = run_to_completion(handler.execute(
            tool_call(),
            ToolContext {
                cancellation: CancellationToken::new(),
                provenance: RunProvenance::default(),
            },
            ToolUpdateSink::disabled(),
        ))
        .expect("handler should complete");
        assert_eq!(result.content, "capability-ok");
        assert_eq!(result.tool_call_id.as_str(), "call-1");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn handler_receives_typed_run_provenance_without_arbitrary_metadata() {
        let capability = Arc::new(EchoCapability {
            calls: Arc::new(AtomicUsize::new(0)),
            response: Ok(CapabilityResponse {
                value: JsonValue::Null,
            }),
        });
        let handler = LuaToolHandler::new(
            "return function(call) return call.provenance.lane_id .. ':' .. call.provenance.source_leaf_id end",
            spec(),
            bindings(capability),
        )
        .expect("handler should load");
        let provenance = RunProvenance {
            lane_id: Some("agent-child".into()),
            source_leaf_id: Some("entry-parent-source".into()),
            ..RunProvenance::default()
        };

        let result = run_to_completion(handler.execute(
            tool_call(),
            ToolContext {
                cancellation: CancellationToken::new(),
                provenance,
            },
            ToolUpdateSink::disabled(),
        ))
        .expect("handler should receive provenance");

        assert_eq!(result.content, "agent-child:entry-parent-source");
    }

    #[test]
    fn handler_exposes_host_execution_policy_to_the_core_scheduler() {
        let capability = Arc::new(EchoCapability {
            calls: Arc::new(AtomicUsize::new(0)),
            response: Ok(CapabilityResponse {
                value: JsonValue::from("unused"),
            }),
        });
        let mut handler_spec = spec();
        handler_spec.requires_exclusive_batch = true;
        handler_spec.cancellation_settlement_mode =
            tea_core::tool::CancellationSettlementMode::AwaitFuture;
        let handler = LuaToolHandler::new(
            "return function(_) return 'ok' end",
            handler_spec,
            bindings(capability),
        )
        .expect("handler should load");

        assert!(handler.requires_exclusive_batch());
        assert_eq!(
            handler.cancellation_settlement_mode(),
            tea_core::tool::CancellationSettlementMode::AwaitFuture
        );
    }

    #[test]
    fn handler_cannot_amplify_host_capability_calls_past_its_budget() {
        let calls = Arc::new(AtomicUsize::new(0));
        let capability = Arc::new(EchoCapability {
            calls: Arc::clone(&calls),
            response: Ok(CapabilityResponse {
                value: JsonValue::from("ok"),
            }),
        });
        let handler = LuaToolHandler::new_with_limits(
            r#"
                return function(call)
                    for _ = 1, 2 do
                        coroutine.yield({
                            kind = "capability",
                            capability = "world",
                            method = "echo",
                            arguments_json = call.arguments_json,
                        })
                    end
                    return { content = "unreachable" }
                end
            "#,
            spec(),
            bindings(capability),
            HandlerLimits {
                max_capability_calls: 1,
                ..HandlerLimits::default()
            },
        )
        .expect("bounded handler should load");

        let error = run_to_completion(handler.execute(
            tool_call(),
            ToolContext {
                cancellation: CancellationToken::new(),
                provenance: RunProvenance::default(),
            },
            ToolUpdateSink::disabled(),
        ))
        .expect_err("the second host operation must be refused");
        assert_eq!(
            error,
            ToolError::Execution {
                tool: "echo_tool".to_owned(),
                message: "Luau handler exceeded its 1 capability-call limit".to_owned(),
            }
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn constructor_rejects_unbound_declared_capability() {
        let error = LuaToolHandler::new(
            "return function(_) return 'ok' end",
            spec(),
            CapabilityBindings::new(),
        )
        .expect_err("an unbound capability must not enter the registry");
        assert_eq!(
            error,
            ToolHandlerInitError::UnboundCapability {
                capability: "world".to_owned()
            }
        );
    }

    #[test]
    fn cancellation_is_reported_as_typed_tool_cancellation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let capability = Arc::new(EchoCapability {
            calls,
            response: Ok(CapabilityResponse {
                value: JsonValue::from("never-used"),
            }),
        });
        let handler = LuaToolHandler::new(
            "return function(_) return 'ok' end",
            spec(),
            bindings(capability),
        )
        .expect("handler should load");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = run_to_completion(handler.execute(
            tool_call(),
            ToolContext {
                cancellation,
                provenance: RunProvenance::default(),
            },
            ToolUpdateSink::disabled(),
        ))
        .expect_err("cancelled handler must not run");
        assert_eq!(
            error,
            ToolError::Cancelled {
                tool: "echo_tool".to_owned()
            }
        );
    }

    #[test]
    fn cancellation_wakes_a_pending_capability_future() {
        let drops = Arc::new(AtomicUsize::new(0));
        let handler = LuaToolHandler::new(
            r#"
                return function(call)
                    local response = coroutine.yield({
                        kind = "capability",
                        capability = "world",
                        method = "never",
                        arguments_json = call.arguments_json,
                    })
                    return { content = response }
                end
            "#,
            spec(),
            bindings(Arc::new(PendingCapability {
                drops: Arc::clone(&drops),
            })),
        )
        .expect("handler should load");
        let cancellation = CancellationToken::new();
        let mut future = handler.execute(
            tool_call(),
            ToolContext {
                cancellation: cancellation.clone(),
                provenance: RunProvenance::default(),
            },
            ToolUpdateSink::disabled(),
        );
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(Pin::new(&mut future).poll(&mut context).is_pending());

        cancellation.cancel();
        let result = match Pin::new(&mut future).poll(&mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("cancellation should settle the pending handler"),
        };
        assert_eq!(
            result,
            Err(ToolError::Cancelled {
                tool: "echo_tool".to_owned()
            })
        );
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    fn collecting_sink() -> (ToolUpdateSink, Arc<std::sync::Mutex<Vec<ToolUpdate>>>) {
        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_updates = Arc::clone(&collected);
        (
            ToolUpdateSink::new(move |update| {
                sink_updates
                    .lock()
                    .expect("update collection is uncontended")
                    .push(update)
            }),
            collected,
        )
    }

    fn denied_capability() -> Arc<dyn LuauCapability> {
        Arc::new(EchoCapability {
            calls: Arc::new(AtomicUsize::new(0)),
            response: Ok(CapabilityResponse {
                value: JsonValue::Null,
            }),
        })
    }

    #[test]
    fn update_yield_publishes_presentation_without_invoking_a_capability() {
        let calls = Arc::new(AtomicUsize::new(0));
        let capability = Arc::new(EchoCapability {
            calls: Arc::clone(&calls),
            response: Ok(CapabilityResponse {
                value: JsonValue::Null,
            }),
        });
        let handler = LuaToolHandler::new(
            r#"
                return function(_)
                    local resumed = coroutine.yield({
                        kind = "update",
                        activity = "Doing something useful",
                    })
                    return { content = "settled:" .. tostring(resumed) }
                end
            "#,
            spec(),
            bindings(capability),
        )
        .expect("handler should load");
        let (sink, collected) = collecting_sink();

        let result = run_to_completion(handler.execute(
            tool_call(),
            ToolContext {
                cancellation: CancellationToken::new(),
                provenance: RunProvenance::default(),
            },
            sink,
        ))
        .expect("handler should complete after publishing an update");

        assert_eq!(result.content, "settled:nil");
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            *collected.lock().expect("collected updates"),
            vec![ToolUpdate {
                content: String::new(),
                details: None,
                activity: Some("Doing something useful".to_owned()),
            }]
        );
    }

    #[test]
    fn update_and_capability_yields_compose_in_one_invocation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let capability = Arc::new(EchoCapability {
            calls: Arc::clone(&calls),
            response: Ok(CapabilityResponse {
                value: JsonValue::from("capability-ok"),
            }),
        });
        let handler = LuaToolHandler::new(
            r#"
                return function(call)
                    coroutine.yield({ kind = "update", content = "progress", activity = "before" })
                    local response = coroutine.yield({
                        kind = "capability",
                        capability = "world",
                        method = "echo",
                        arguments_json = call.arguments_json,
                    })
                    coroutine.yield({ kind = "update", activity = "after" })
                    return { content = response }
                end
            "#,
            spec(),
            bindings(capability),
        )
        .expect("handler should load");
        let (sink, collected) = collecting_sink();

        let result = run_to_completion(handler.execute(
            tool_call(),
            ToolContext {
                cancellation: CancellationToken::new(),
                provenance: RunProvenance::default(),
            },
            sink,
        ))
        .expect("handler should complete");

        assert_eq!(result.content, "capability-ok");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let collected = collected.lock().expect("collected updates");
        assert_eq!(
            collected
                .iter()
                .map(|update| (update.content.as_str(), update.activity.as_deref()))
                .collect::<Vec<_>>(),
            [("progress", Some("before")), ("", Some("after"))],
        );
    }

    #[test]
    fn presentation_yields_share_one_finite_budget_with_capability_calls() {
        let handler = LuaToolHandler::new_with_limits(
            r#"
                return function(_)
                    for _ = 1, 4 do
                        coroutine.yield({ kind = "update", activity = "loop" })
                    end
                    return { content = "unreachable" }
                end
            "#,
            spec(),
            bindings(denied_capability()),
            HandlerLimits {
                max_capability_calls: 2,
                ..HandlerLimits::default()
            },
        )
        .expect("bounded handler should load");
        let (sink, collected) = collecting_sink();

        let error = run_to_completion(handler.execute(
            tool_call(),
            ToolContext {
                cancellation: CancellationToken::new(),
                provenance: RunProvenance::default(),
            },
            sink,
        ))
        .expect_err("an unbounded host-update loop must be refused");
        assert_eq!(
            error,
            ToolError::Execution {
                tool: "echo_tool".to_owned(),
                message: "Luau handler exceeded its 2 yield limit".to_owned(),
            }
        );
        assert_eq!(collected.lock().expect("collected updates").len(), 2);
    }

    #[test]
    fn oversized_activity_payloads_are_refused() {
        let handler = LuaToolHandler::new(
            format!(
                "return function(_) coroutine.yield({{ kind = \"update\", activity = string.rep(\"a\", {}) }}) return 'unreachable' end",
                MAX_ACTIVITY_BYTES + 1
            ),
            spec(),
            bindings(denied_capability()),
        )
        .expect("handler should load");
        let (sink, collected) = collecting_sink();

        let error = run_to_completion(handler.execute(
            tool_call(),
            ToolContext {
                cancellation: CancellationToken::new(),
                provenance: RunProvenance::default(),
            },
            sink,
        ))
        .expect_err("an oversized presentation payload must be refused");
        assert_eq!(
            error,
            ToolError::Execution {
                tool: "echo_tool".to_owned(),
                message: format!("update activity exceeds {MAX_ACTIVITY_BYTES} bytes"),
            }
        );
        assert!(collected.lock().expect("collected updates").is_empty());
    }

    #[test]
    fn malformed_update_yields_are_rejected_deterministically() {
        for (source, message) in [
            (
                r#"coroutine.yield({ kind = "update" })"#,
                "update yield requires content, activity, or both",
            ),
            (
                r#"coroutine.yield({ kind = "update", activity = 7 })"#,
                "update activity must be a string",
            ),
            (
                r#"coroutine.yield({ kind = "update", activity = "ok", capability = "world" })"#,
                "update yield has unknown field \"capability\"",
            ),
            (
                r#"coroutine.yield({ kind = "presentation", activity = "ok" })"#,
                "handler yielded unsupported request kind \"presentation\"",
            ),
        ] {
            let handler = LuaToolHandler::new(
                format!("return function(_) {source} return 'unreachable' end"),
                spec(),
                bindings(denied_capability()),
            )
            .expect("handler should load");
            let (sink, collected) = collecting_sink();
            let error = run_to_completion(handler.execute(
                tool_call(),
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: RunProvenance::default(),
                },
                sink,
            ))
            .expect_err("a malformed update yield must not settle successfully");
            let ToolError::Execution {
                message: actual, ..
            } = error
            else {
                panic!("a malformed update yield is an execution contract failure");
            };
            assert!(
                actual.starts_with(message),
                "expected {actual:?} to start with {message:?}"
            );
            assert!(collected.lock().expect("collected updates").is_empty());
        }
    }

    #[test]
    fn cancellation_still_settles_a_handler_that_publishes_presentation() {
        let handler = LuaToolHandler::new(
            r#"
                return function(call)
                    coroutine.yield({ kind = "update", activity = "starting" })
                    local response = coroutine.yield({
                        kind = "capability",
                        capability = "world",
                        method = "never",
                        arguments_json = call.arguments_json,
                    })
                    return { content = response }
                end
            "#,
            spec(),
            bindings(Arc::new(PendingCapability {
                drops: Arc::new(AtomicUsize::new(0)),
            })),
        )
        .expect("handler should load");
        let cancellation = CancellationToken::new();
        let (sink, collected) = collecting_sink();
        let mut future = handler.execute(
            tool_call(),
            ToolContext {
                cancellation: cancellation.clone(),
                provenance: RunProvenance::default(),
            },
            sink,
        );
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(Pin::new(&mut future).poll(&mut context).is_pending());
        assert_eq!(collected.lock().expect("collected updates").len(), 1);

        cancellation.cancel();
        let Poll::Ready(result) = Pin::new(&mut future).poll(&mut context) else {
            panic!("cancellation should settle the pending handler");
        };
        assert_eq!(
            result,
            Err(ToolError::Cancelled {
                tool: "echo_tool".to_owned()
            })
        );
    }

    #[test]
    fn handler_result_can_carry_validated_details_and_stop_flag() {
        let capability = Arc::new(EchoCapability {
            calls: Arc::new(AtomicUsize::new(0)),
            response: Ok(CapabilityResponse {
                value: JsonValue::Null,
            }),
        });
        let handler = LuaToolHandler::new(
            "return function(_) return { content = 'done', details_json = '{\\\"ok\\\":true}', terminate = true, is_error = false } end",
            spec(),
            bindings(capability),
        )
        .expect("handler should load");
        let result = run_to_completion(handler.execute(
            tool_call(),
            ToolContext {
                cancellation: CancellationToken::new(),
                provenance: RunProvenance::default(),
            },
            ToolUpdateSink::disabled(),
        ))
        .expect("handler should complete");
        assert_eq!(result.content, "done");
        assert_eq!(result.details, Some(SerializedJson::new(r#"{"ok":true}"#)));
        assert!(result.terminate);
        assert!(!result.is_error);
    }
}
