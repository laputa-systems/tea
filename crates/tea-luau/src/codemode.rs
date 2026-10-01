//! Luau codemode: a model-authored script composes other tool calls.
//!
//! Adapted from upstream Pi's codemode (`packages/coding-agent/src/extensions/
//! codemode`), which runs model-authored JavaScript in QuickJS. Tea keeps the
//! concept — a script calls tools, processes intermediate results, and returns
//! only useful output — and implements it with the existing sandboxed Luau
//! runtime instead.
//!
//! A script gets a deliberately small surface:
//!
//! | Global | Purpose |
//! | --- | --- |
//! | `call(name, args)` | Call a tool; returns its text or raises its error. |
//! | `try_call(name, args)` | Call a tool; returns `ok, text`. |
//! | `parallel({{name, args}, ...})` | Run calls concurrently; returns `{ok=, text=}` per call. |
//! | `tools.<name>(args)` | Shorthand for `call`. |
//! | `search_tools(query, limit)` | BM25 search over callable tools: `{name=, description=}`. |
//! | `describe_tool(name)` | `{name=, description=, schema=}` or `nil`. |
//! | `print(...)` | Append a line to the output. |
//! | `json.encode(v)`, `json.decode(s)` | JSON helpers. |
//!
//! Every call goes through [`tea_core::tool::ToolComposition`], so the owning
//! run validates, policy-checks, attributes, and settles it exactly like a
//! model-issued call. The script has no file, process, network, module, or
//! extension-host authority. Execution is bounded by source size, VM memory,
//! an instruction budget per resume, a call budget, and an output budget. A
//! failed script keeps its partial output and does not undo calls that already
//! ran; calls still running settle before the result is returned.

use crate::tool_handler::{json_to_lua, lua_to_json};
use mlua::thread::ThreadStatus;
use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Table, Thread, Value};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tea_core::error::ToolError;
use tea_core::scheduler::CancellationWait;
use tea_core::state::SerializedJson;
use tea_core::tool::{
    AgentTool, AgentToolResult, CompositionAccess, NestedCallFuture, ToolCall, ToolComposition,
    ToolContext, ToolDeclaration, ToolExecutionMode, ToolFuture, ToolUpdate, ToolUpdateSink,
};
use tea_core::tool_search::{rank, ToolSearchDocument};
use tea_protocol::JsonValue;

/// Name of the codemode tool.
pub const CODEMODE_TOOL_NAME: &str = "codemode";

const CHUNK_NAME: &str = "codemode.luau";
const REQUEST_MARKER: &str = "__codemode_request";

/// Finite resource limits for one script.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodemodeLimits {
    /// Largest accepted script in bytes.
    pub max_source_bytes: usize,
    /// Largest Luau VM allocation total in bytes.
    pub max_memory_bytes: usize,
    /// Interrupt checks permitted per resume (bounds pure computation).
    pub max_interrupt_checks: usize,
    /// Largest number of tool calls per script.
    pub max_calls: usize,
    /// Largest number of calls in one `parallel` batch.
    pub max_parallel: usize,
    /// Largest output returned to the model; longer output keeps its start
    /// and end around an omission marker.
    pub max_output_bytes: usize,
}

impl Default for CodemodeLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 32 * 1024,
            max_memory_bytes: 32 * 1024 * 1024,
            max_interrupt_checks: 200_000,
            max_calls: 64,
            max_parallel: 8,
            max_output_bytes: 16 * 1024,
        }
    }
}

/// The optional codemode composition tool.
pub struct CodemodeTool {
    limits: CodemodeLimits,
    description: String,
    schema: JsonValue,
}

impl fmt::Debug for CodemodeTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodemodeTool")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// Largest tool listing embedded in the description, in bytes.
const LISTING_BUDGET: usize = 6 * 1024;

impl CodemodeTool {
    /// Build the tool. `listed` are the tools named in the description
    /// (typically the direct and composition-only tools); deferred tools are
    /// left out so the description stays stable as optional tools appear, and
    /// scripts find them with `search_tools`.
    pub fn new(listed: &[ToolDeclaration], limits: CodemodeLimits) -> Self {
        Self {
            limits,
            description: description(listed),
            schema: JsonValue::object([
                ("type", JsonValue::from("object")),
                (
                    "properties",
                    JsonValue::object([(
                        "script",
                        JsonValue::object([
                            ("type", JsonValue::from("string")),
                            (
                                "description",
                                JsonValue::from("Luau source. Top-level `return` adds its value to the output."),
                            ),
                        ]),
                    )]),
                ),
                ("required", JsonValue::Array(vec![JsonValue::from("script")])),
                ("additionalProperties", JsonValue::Bool(false)),
            ]),
        }
    }
}

fn description(listed: &[ToolDeclaration]) -> String {
    let mut text = String::from(
        "Run a Luau script that calls other tools and returns only what matters. \
Use it to chain calls, run independent calls concurrently, and filter large results before you see them.\n\n\
Globals: call(name, args) returns a tool's text or raises its error; try_call(name, args) returns ok, text; \
parallel({{name, args}, ...}) runs calls concurrently and returns {ok=, text=} per call; tools.<name>(args) is call(name, args); \
search_tools(query, limit) finds tools that are not listed here, such as MCP tools; describe_tool(name) returns {name, description, schema}; \
print(...) adds output; json.encode/json.decode convert values. A top-level return adds its value to the output.\n\n\
Scripts can call the tools declared to you by name, plus tools found with search_tools. Only the output reaches you. Tool calls are real and are not undone if the script fails later. \
There is no file, process, or network access except through tools.",
    );
    if listed.is_empty() {
        return text;
    }
    text.push_str("\n\nTools:");
    let mut used = text.len();
    let mut omitted = 0;
    for tool in listed {
        let summary = tool.description.trim().lines().next().unwrap_or_default();
        let properties = tool
            .schema
            .get("properties")
            .and_then(JsonValue::as_object)
            .map(|properties| properties.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        let line = format!("\n- {}({{{properties}}}): {summary}", tool.name);
        if used + line.len() > LISTING_BUDGET {
            omitted += 1;
            continue;
        }
        used += line.len();
        text.push_str(&line);
    }
    if omitted > 0 {
        text.push_str(&format!(
            "\n({omitted} more tools not listed; use search_tools.)"
        ));
    }
    text
}

impl AgentTool for CodemodeTool {
    fn name(&self) -> &str {
        CODEMODE_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> &JsonValue {
        &self.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // Nested ordering is enforced per script; scripts never overlap.
        ToolExecutionMode::Sequential
    }

    fn composition_access(&self) -> CompositionAccess {
        CompositionAccess::Calls
    }

    fn script_callable(&self) -> bool {
        false
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let composition = context.composition.clone().ok_or_else(|| ToolError::Execution {
                tool: CODEMODE_TOOL_NAME.into(),
                message: "codemode requires the run's composition facility".into(),
            })?;
            let script = JsonValue::parse(call.arguments.as_str())
                .ok()
                .and_then(|arguments| arguments.get("script").and_then(JsonValue::as_str).map(str::to_owned))
                .ok_or_else(|| ToolError::InvalidArguments {
                    tool: CODEMODE_TOOL_NAME.into(),
                    message: "script must be a string".into(),
                })?;
            if script.len() > self.limits.max_source_bytes {
                return Err(ToolError::InvalidArguments {
                    tool: CODEMODE_TOOL_NAME.into(),
                    message: format!(
                        "script is {} bytes; the limit is {}",
                        script.len(),
                        self.limits.max_source_bytes
                    ),
                });
            }
            let execution = ScriptExecution::start(&script, composition, self.limits, updates)
                .map_err(|message| ToolError::InvalidArguments {
                    tool: CODEMODE_TOOL_NAME.into(),
                    message,
                })?;
            let outcome = Drive {
                execution,
                pending: None,
                cancellation: context.cancellation.cancelled(),
                cancelled: context.cancellation.clone(),
            }
            .await?;
            Ok(outcome.into_result(call, self.limits))
        })
    }
}

/// Shared, bounded script output.
#[derive(Default)]
struct Output {
    text: String,
    dropped: usize,
}

impl Output {
    fn push(&mut self, line: &str, limit: usize) {
        // Keep at most twice the returned budget in memory; the middle is
        // omitted on return anyway.
        let budget = limit.saturating_mul(2);
        if self.text.len() + line.len() + 1 > budget {
            self.dropped += line.len() + 1;
            return;
        }
        self.text.push_str(line);
        self.text.push('\n');
    }
}

struct ScriptExecution {
    _lua: Lua,
    thread: Thread,
    output: Arc<Mutex<Output>>,
    budget: Arc<AtomicUsize>,
    composition: ToolComposition,
    limits: CodemodeLimits,
    updates: ToolUpdateSink,
    resume: Option<MultiValue>,
    calls: usize,
    failed_calls: usize,
}

struct ScriptOutcome {
    output: Output,
    error: Option<String>,
    calls: usize,
    failed_calls: usize,
}

impl ScriptOutcome {
    fn into_result(self, call: ToolCall, limits: CodemodeLimits) -> AgentToolResult {
        let mut output = self.output.text.trim_end().to_owned();
        if self.output.dropped > 0 {
            output.push_str(&format!(
                "\n[{} more output bytes were discarded]",
                self.output.dropped
            ));
        }
        let output = tea_core::tool::truncate_middle(&output, limits.max_output_bytes);
        let calls = format!(
            "{} tool call{} ({} failed)",
            self.calls,
            if self.calls == 1 { "" } else { "s" },
            self.failed_calls
        );
        let body = if output.is_empty() {
            "(no output)".to_owned()
        } else {
            output
        };
        let (content, is_error) = match self.error {
            None => (format!("Script completed after {calls}.\n{body}"), false),
            Some(error) => (
                format!(
                    "Script failed after {calls}. Calls that ran are not undone.\n{body}\nScript error: {}",
                    tea_core::tool::truncate_middle(&error, 2_048)
                ),
                true,
            ),
        };
        AgentToolResult {
            tool_call_id: call.id,
            content,
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error,
            failure: is_error.then(tea_core::tool::ToolFailure::recoverable),
        }
    }
}

enum Request {
    Call { name: String, arguments: JsonValue },
    Parallel(Vec<(String, JsonValue)>),
}

enum Pending {
    Call(NestedCallFuture),
    Parallel(Vec<(NestedCallFuture, Option<AgentToolResult>)>),
}

const PRELUDE: &str = r#"
local make = __codemode_make_request
local yield = coroutine.yield
function try_call(name, args)
    return yield(make("call", name, args))
end
function call(name, args)
    local ok, text = yield(make("call", name, args))
    if not ok then
        error(text, 2)
    end
    return text
end
function parallel(calls)
    return yield(make("parallel", nil, calls))
end
tools = setmetatable({}, {
    __index = function(_, name)
        return function(args)
            return call(name, args)
        end
    end,
})
"#;

impl ScriptExecution {
    fn start(
        script: &str,
        composition: ToolComposition,
        limits: CodemodeLimits,
        updates: ToolUpdateSink,
    ) -> Result<Self, String> {
        let lua = Lua::new_with(
            StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING | StdLib::UTF8 | StdLib::MATH,
            LuaOptions::new(),
        )
        .map_err(|error| error.to_string())?;
        lua.set_memory_limit(limits.max_memory_bytes)
            .map_err(|error| error.to_string())?;
        let output = Arc::new(Mutex::new(Output::default()));
        install_globals(&lua, &composition, &output, limits).map_err(|error| error.to_string())?;
        lua.load(PRELUDE)
            .set_name("codemode-prelude.luau")
            .exec()
            .map_err(|error| error.to_string())?;
        lua.sandbox(true).map_err(|error| error.to_string())?;
        let budget = Arc::new(AtomicUsize::new(limits.max_interrupt_checks));
        let counter = Arc::clone(&budget);
        lua.set_interrupt(move |_| {
            if counter
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_err()
            {
                return Err(mlua::Error::RuntimeError(
                    "script exceeded its computation budget between tool calls".into(),
                ));
            }
            Ok(mlua::VmState::Continue)
        });
        let function: Function = lua
            .load(script)
            .set_name(CHUNK_NAME)
            .into_function()
            .map_err(|error| format!("script does not compile: {error}"))?;
        let thread = lua
            .create_thread(function)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            _lua: lua,
            thread,
            output,
            budget,
            composition,
            limits,
            updates,
            resume: Some(MultiValue::new()),
            calls: 0,
            failed_calls: 0,
        })
    }

    fn finish(&mut self, error: Option<String>) -> ScriptOutcome {
        ScriptOutcome {
            output: std::mem::take(&mut *self.output.lock().expect("codemode output poisoned")),
            error,
            calls: self.calls,
            failed_calls: self.failed_calls,
        }
    }

    /// Resume the script once. Returns the next request, or the outcome.
    fn step(&mut self) -> Result<Request, ScriptOutcome> {
        let arguments = self.resume.take().unwrap_or_default();
        self.budget
            .store(self.limits.max_interrupt_checks, Ordering::Relaxed);
        let values = match self.thread.resume::<MultiValue>(arguments) {
            Ok(values) => values,
            Err(error) => return Err(self.finish(Some(script_error(&error)))),
        };
        if self.thread.status() != ThreadStatus::Resumable {
            for value in values {
                if let Err(error) = self.append_value(value) {
                    return Err(self.finish(Some(error)));
                }
            }
            return Err(self.finish(None));
        }
        match parse_request(values) {
            Ok(request) => Ok(request),
            Err(error) => Err(self.finish(Some(error))),
        }
    }

    fn append_value(&mut self, value: Value) -> Result<(), String> {
        let text = match value {
            Value::Nil => return Ok(()),
            Value::String(text) => text.to_string_lossy().to_string(),
            other => lua_to_json(other)?
                .to_json_string()
                .map_err(|error| error.to_string())?,
        };
        self.output
            .lock()
            .expect("codemode output poisoned")
            .push(&text, self.limits.max_output_bytes);
        Ok(())
    }

    fn submit(&mut self, request: Request) -> Result<Pending, String> {
        let count = match &request {
            Request::Call { .. } => 1,
            Request::Parallel(calls) => calls.len(),
        };
        if self.calls + count > self.limits.max_calls {
            return Err(format!(
                "script exceeded its limit of {} tool calls",
                self.limits.max_calls
            ));
        }
        self.calls += count;
        let submit = |name: &str, arguments: &JsonValue| -> Result<NestedCallFuture, String> {
            let arguments = arguments.to_json_string().map_err(|error| error.to_string())?;
            self.updates.emit(ToolUpdate {
                content: format!("→ {name}"),
                details: None,
                activity: Some(format!("codemode → {name}")),
            });
            Ok(self.composition.call(name, SerializedJson::new(arguments)))
        };
        match request {
            Request::Call { name, arguments } => Ok(Pending::Call(submit(&name, &arguments)?)),
            Request::Parallel(calls) => Ok(Pending::Parallel(
                calls
                    .iter()
                    .map(|(name, arguments)| submit(name, arguments).map(|future| (future, None)))
                    .collect::<Result<_, _>>()?,
            )),
        }
    }

    fn deliver_call(&mut self, result: AgentToolResult) -> Result<(), String> {
        if result.is_error {
            self.failed_calls += 1;
        }
        let lua = &self._lua;
        let values = MultiValue::from_vec(vec![
            Value::Boolean(!result.is_error),
            Value::String(lua.create_string(&result.content).map_err(|error| error.to_string())?),
        ]);
        self.resume = Some(values);
        Ok(())
    }

    fn deliver_parallel(&mut self, results: Vec<AgentToolResult>) -> Result<(), String> {
        let lua = &self._lua;
        let table = lua.create_table().map_err(|error| error.to_string())?;
        for (index, result) in results.into_iter().enumerate() {
            if result.is_error {
                self.failed_calls += 1;
            }
            let entry = lua.create_table().map_err(|error| error.to_string())?;
            entry.set("ok", !result.is_error).map_err(|error| error.to_string())?;
            entry.set("text", result.content).map_err(|error| error.to_string())?;
            table.set(index + 1, entry).map_err(|error| error.to_string())?;
        }
        self.resume = Some(MultiValue::from_vec(vec![Value::Table(table)]));
        Ok(())
    }
}

fn script_error(error: &mlua::Error) -> String {
    match error {
        mlua::Error::RuntimeError(message) => message.clone(),
        mlua::Error::CallbackError { cause, .. } => script_error(cause),
        mlua::Error::MemoryError(_) => "script exceeded its memory limit".into(),
        other => other.to_string(),
    }
}

fn parse_request(values: MultiValue) -> Result<Request, String> {
    let Some(Value::Table(table)) = values.into_iter().next() else {
        return Err("scripts may only yield through call, try_call, or parallel".into());
    };
    if table.get::<Option<bool>>(REQUEST_MARKER).ok().flatten() != Some(true) {
        return Err("scripts may only yield through call, try_call, or parallel".into());
    }
    let kind: String = table.get("kind").map_err(|error| error.to_string())?;
    match kind.as_str() {
        "call" => {
            let name: String = table
                .get("name")
                .map_err(|_| "call requires a tool name string".to_owned())?;
            let arguments = arguments_json(table.get::<Value>("args").map_err(|error| error.to_string())?)?;
            Ok(Request::Call { name, arguments })
        }
        "parallel" => {
            let Value::Table(calls) = table.get::<Value>("args").map_err(|error| error.to_string())? else {
                return Err("parallel requires a list of {name, args} calls".into());
            };
            let mut parsed = Vec::new();
            for entry in calls.sequence_values::<Value>() {
                let Value::Table(entry) = entry.map_err(|error| error.to_string())? else {
                    return Err("each parallel call must be a {name, args} table".into());
                };
                let name = entry
                    .get::<Option<String>>("name")
                    .ok()
                    .flatten()
                    .or_else(|| entry.get::<Option<String>>(1).ok().flatten())
                    .ok_or_else(|| "each parallel call needs a tool name".to_owned())?;
                let arguments = match entry.get::<Value>("args").map_err(|error| error.to_string())? {
                    Value::Nil => entry.get::<Value>(2).map_err(|error| error.to_string())?,
                    value => value,
                };
                parsed.push((name, arguments_json(arguments)?));
            }
            Ok(Request::Parallel(parsed))
        }
        other => Err(format!("unknown codemode request {other:?}")),
    }
}

fn arguments_json(value: Value) -> Result<JsonValue, String> {
    match value {
        Value::Nil => Ok(JsonValue::object(Vec::<(&str, JsonValue)>::new())),
        Value::Table(_) => match lua_to_json(value)? {
            // An empty Luau table is an empty argument object.
            JsonValue::Array(values) if values.is_empty() => {
                Ok(JsonValue::object(Vec::<(&str, JsonValue)>::new()))
            }
            json @ JsonValue::Object(_) => Ok(json),
            _ => Err("tool arguments must be a table of named fields".into()),
        },
        other => Err(format!(
            "tool arguments must be a table, not {}",
            other.type_name()
        )),
    }
}

fn install_globals(
    lua: &Lua,
    composition: &ToolComposition,
    output: &Arc<Mutex<Output>>,
    limits: CodemodeLimits,
) -> mlua::Result<()> {
    let globals = lua.globals();
    // Scripts load no modules; remove the loader entry point outright.
    globals.set("require", Value::Nil)?;
    globals.set(
        "__codemode_make_request",
        lua.create_function(|lua, (kind, name, args): (String, Option<String>, Value)| {
            let request = lua.create_table()?;
            request.set(REQUEST_MARKER, true)?;
            request.set("kind", kind)?;
            request.set("name", name)?;
            request.set("args", args)?;
            Ok(request)
        })?,
    )?;
    let printed = Arc::clone(output);
    globals.set(
        "print",
        lua.create_function(move |_, values: MultiValue| {
            let line = values
                .into_iter()
                .map(|value| match value {
                    Value::String(text) => text.to_string_lossy().to_string(),
                    Value::Nil => "nil".into(),
                    Value::Boolean(value) => value.to_string(),
                    Value::Integer(value) => value.to_string(),
                    Value::Number(value) => value.to_string(),
                    other => lua_to_json(other)
                        .ok()
                        .and_then(|json| json.to_json_string().ok())
                        .unwrap_or_else(|| "<value>".into()),
                })
                .collect::<Vec<_>>()
                .join("\t");
            printed
                .lock()
                .expect("codemode output poisoned")
                .push(&line, limits.max_output_bytes);
            Ok(())
        })?,
    )?;
    let json = lua.create_table()?;
    json.set(
        "encode",
        lua.create_function(|_, value: Value| {
            lua_to_json(value)
                .map_err(mlua::Error::RuntimeError)?
                .to_json_string()
                .map_err(|error| mlua::Error::RuntimeError(error.to_string()))
        })?,
    )?;
    json.set(
        "decode",
        lua.create_function(|lua, text: String| {
            let value = JsonValue::parse(&text)
                .map_err(|error| mlua::Error::RuntimeError(format!("invalid JSON: {error}")))?;
            json_to_lua(lua, &value)
        })?,
    )?;
    globals.set("json", json)?;

    let callable = composition
        .catalog()
        .iter()
        .filter(|entry| entry.script_callable)
        .map(|entry| entry.declaration.clone())
        .collect::<Vec<_>>();
    let documents = Arc::new(
        callable
            .iter()
            .map(ToolSearchDocument::from_declaration)
            .collect::<Vec<_>>(),
    );
    let declarations = Arc::new(callable);
    let search_declarations = Arc::clone(&declarations);
    globals.set(
        "search_tools",
        lua.create_function(move |lua, (query, limit): (String, Option<usize>)| {
            let found = rank(&query, &documents, limit.unwrap_or(8).clamp(1, 32));
            let table = lua.create_table()?;
            for (index, found) in found.iter().enumerate() {
                let description = search_declarations
                    .iter()
                    .find(|declaration| declaration.name == found.name)
                    .map(|declaration| declaration.description.clone())
                    .unwrap_or_default();
                let entry = lua.create_table()?;
                entry.set("name", found.name.as_str())?;
                entry.set("description", description)?;
                table.set(index + 1, entry)?;
            }
            Ok(table)
        })?,
    )?;
    globals.set(
        "describe_tool",
        lua.create_function(move |lua, name: String| {
            let Some(declaration) = declarations.iter().find(|declaration| declaration.name == name)
            else {
                return Ok(Value::Nil);
            };
            let table: Table = lua.create_table()?;
            table.set("name", declaration.name.as_str())?;
            table.set("description", declaration.description.as_str())?;
            table.set("schema", json_to_lua(lua, &declaration.schema)?)?;
            Ok(Value::Table(table))
        })?,
    )?;
    Ok(())
}

/// Drives a script: resume, submit its calls, await them, resume again.
struct Drive {
    execution: ScriptExecution,
    pending: Option<Pending>,
    cancellation: CancellationWait,
    cancelled: tea_core::scheduler::CancellationToken,
}

impl Future for Drive {
    type Output = Result<ScriptOutcome, ToolError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            if this.cancelled.is_cancelled()
                || Pin::new(&mut this.cancellation).poll(context).is_ready()
            {
                // Dropping pending call futures does not abandon the calls:
                // the owning run settles every started call.
                this.pending = None;
                return Poll::Ready(Err(ToolError::Cancelled {
                    tool: CODEMODE_TOOL_NAME.into(),
                }));
            }
            match this.pending.take() {
                Some(Pending::Call(mut future)) => match Pin::new(&mut future).poll(context) {
                    Poll::Pending => {
                        this.pending = Some(Pending::Call(future));
                        return Poll::Pending;
                    }
                    Poll::Ready(result) => {
                        if let Err(error) = this.execution.deliver_call(result) {
                            return Poll::Ready(Ok(this.execution.finish(Some(error))));
                        }
                    }
                },
                Some(Pending::Parallel(mut calls)) => {
                    for (future, result) in &mut calls {
                        if result.is_none() {
                            if let Poll::Ready(value) = Pin::new(future).poll(context) {
                                *result = Some(value);
                            }
                        }
                    }
                    if calls.iter().any(|(_, result)| result.is_none()) {
                        this.pending = Some(Pending::Parallel(calls));
                        return Poll::Pending;
                    }
                    let results = calls
                        .into_iter()
                        .map(|(_, result)| result.expect("every call settled"))
                        .collect();
                    if let Err(error) = this.execution.deliver_parallel(results) {
                        return Poll::Ready(Ok(this.execution.finish(Some(error))));
                    }
                }
                None => {}
            }
            let request = match this.execution.step() {
                Ok(request) => request,
                Err(outcome) => return Poll::Ready(Ok(outcome)),
            };
            if let Request::Parallel(calls) = &request {
                if calls.len() > this.execution.limits.max_parallel {
                    let error = format!(
                        "parallel accepts at most {} calls",
                        this.execution.limits.max_parallel
                    );
                    return Poll::Ready(Ok(this.execution.finish(Some(error))));
                }
            }
            match this.execution.submit(request) {
                Ok(pending) => this.pending = Some(pending),
                Err(error) => return Poll::Ready(Ok(this.execution.finish(Some(error)))),
            }
        }
    }
}

#[cfg(test)]
mod tests;
