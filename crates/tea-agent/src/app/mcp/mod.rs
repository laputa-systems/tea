//! Host-side MCP client for explicitly configured local stdio servers.
//!
//! Protocol handling, server processes, configuration, and any credentials
//! stay in the terminal host; `tea-core` only sees ordinary trusted tools
//! through [`DynamicToolSource`]. Servers start in the background when a
//! session's runtime first needs them, so an unused or slow server never
//! blocks startup. Each epoch snapshots the tools of servers that are ready.
//!
//! MCP tools default to deferred exposure: they are found with `tool_search`
//! (or a codemode script) instead of being declared in every prompt.
//!
//! Out of scope: remote HTTP transports, OAuth, provider-token sharing,
//! ambient configuration discovery, and server marketplaces.

pub(crate) mod protocol;
#[cfg(any(test, feature = "mcp-fixture"))]
pub mod fixture;

use protocol::{Connection, RpcError};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tea_core::error::ToolError;
use tea_core::runtime::DynamicToolSource;
use tea_core::state::SerializedJson;
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolExposure, ToolFuture, ToolUpdateSink,
};
use tea_protocol::JsonValue;

/// Protocol revision this client speaks.
pub(crate) const PROTOCOL_VERSION: &str = "2025-06-18";

/// Environment variables a server inherits from tea. Everything else,
/// including credentials, must be configured explicitly per server.
const INHERITED_ENVIRONMENT: [&str; 8] = [
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR", "TERM",
];

/// How long an epoch waits for servers that are still starting.
const STARTUP_GRACE: Duration = Duration::from_secs(5);
/// How long shutdown waits for a server to exit after its input closes.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// One configured server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct McpServerConfig {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) exposure: ToolExposure,
    pub(crate) startup_timeout: Duration,
    pub(crate) call_timeout: Duration,
}

/// Observable server state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ServerStatus {
    Starting,
    Ready { tools: usize },
    Failed(String),
    Exited(String),
}

struct Running {
    child: Child,
    connection: Arc<Connection>,
    reader: Option<std::thread::JoinHandle<()>>,
    stderr: Option<std::thread::JoinHandle<()>>,
}

struct ServerState {
    status: ServerStatus,
    tools: Vec<Arc<dyn AgentTool>>,
    running: Option<Running>,
    shut_down: bool,
}

struct Server {
    config: McpServerConfig,
    state: Mutex<ServerState>,
    changed: Condvar,
    stderr_tail: Arc<Mutex<std::collections::VecDeque<String>>>,
}

/// The runtime-owned set of configured MCP servers.
pub(crate) struct McpManager {
    servers: Vec<Arc<Server>>,
    started_at: Instant,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpManager")
            .field("servers", &self.statuses())
            .finish()
    }
}

impl McpManager {
    /// Start every configured server in the background.
    pub(crate) fn start(configs: Vec<McpServerConfig>, workspace: PathBuf) -> Arc<Self> {
        let servers = configs
            .into_iter()
            .map(|config| {
                Arc::new(Server {
                    config,
                    state: Mutex::new(ServerState {
                        status: ServerStatus::Starting,
                        tools: Vec::new(),
                        running: None,
                        shut_down: false,
                    }),
                    changed: Condvar::new(),
                    stderr_tail: Arc::default(),
                })
            })
            .collect::<Vec<_>>();
        for server in &servers {
            let server = Arc::clone(server);
            let workspace = workspace.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("tea-mcp-start-{}", server.config.name))
                .spawn(move || server.connect(&workspace));
            if let Err(error) = spawned {
                eprintln!("warning: could not start an MCP startup thread: {error}");
            }
        }
        Arc::new(Self {
            servers,
            started_at: Instant::now(),
        })
    }

    /// Current status of each server, in configuration order.
    pub(crate) fn statuses(&self) -> Vec<(String, ServerStatus)> {
        self.servers
            .iter()
            .map(|server| {
                let mut state = server.state.lock().expect("MCP server poisoned");
                server.observe_exit(&mut state);
                (server.config.name.clone(), state.status.clone())
            })
            .collect()
    }

    /// Process ids of running servers, for lifecycle tests.
    #[cfg(test)]
    pub(crate) fn pids(&self) -> Vec<u32> {
        self.servers
            .iter()
            .filter_map(|server| {
                let state = server.state.lock().expect("MCP server poisoned");
                state.running.as_ref().map(|running| running.child.id())
            })
            .collect()
    }

    /// Close every server: input first, then a bounded wait, then kill.
    pub(crate) fn shutdown(&self) {
        for server in &self.servers {
            server.shutdown();
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl DynamicToolSource for McpManager {
    fn snapshot(&self) -> Vec<Arc<dyn AgentTool>> {
        let deadline = self.started_at + STARTUP_GRACE;
        let mut tools = Vec::new();
        for server in &self.servers {
            let mut state = server.state.lock().expect("MCP server poisoned");
            while state.status == ServerStatus::Starting {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                state = server
                    .changed
                    .wait_timeout(state, remaining)
                    .expect("MCP server poisoned")
                    .0;
            }
            server.observe_exit(&mut state);
            let refresh = matches!(state.status, ServerStatus::Ready { .. })
                && state
                    .running
                    .as_ref()
                    .is_some_and(|running| running.connection.take_tools_changed());
            if refresh {
                if let Some(connection) = state
                    .running
                    .as_ref()
                    .map(|running| Arc::clone(&running.connection))
                {
                    match list_tools(&server.config, &connection) {
                        Ok(listed) => {
                            state.status = ServerStatus::Ready {
                                tools: listed.len(),
                            };
                            state.tools = listed;
                        }
                        Err(error) => server.diagnostic(format!("tool list refresh failed: {error}")),
                    }
                }
            }
            if matches!(state.status, ServerStatus::Ready { .. }) {
                tools.extend(state.tools.iter().cloned());
            }
        }
        tools
    }
}

impl Server {
    fn diagnostic(&self, line: String) {
        let mut tail = self.stderr_tail.lock().expect("MCP stderr poisoned");
        if tail.len() == 32 {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    fn fail(&self, message: String) {
        let mut state = self.state.lock().expect("MCP server poisoned");
        let tail = self
            .stderr_tail
            .lock()
            .expect("MCP stderr poisoned")
            .iter()
            .rev()
            .take(3)
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        let message = if tail.is_empty() {
            message
        } else {
            format!("{message} (stderr: {})", tail.join(" | "))
        };
        state.status = ServerStatus::Failed(message);
        state.tools.clear();
        if let Some(running) = state.running.take() {
            stop(running);
        }
        drop(state);
        self.changed.notify_all();
    }

    fn connect(self: &Arc<Self>, workspace: &std::path::Path) {
        let mut command = Command::new(&self.config.command);
        command
            .args(&self.config.args)
            .current_dir(self.config.cwd.as_deref().unwrap_or(workspace))
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for variable in INHERITED_ENVIRONMENT {
            if let Some(value) = std::env::var_os(variable) {
                command.env(variable, value);
            }
        }
        command.envs(&self.config.env);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.fail(format!("could not start {:?}: {error}", self.config.command));
                return;
            }
        };
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            self.fail("server pipes were unavailable".into());
            return;
        };
        let tail = Arc::clone(&self.stderr_tail);
        let stderr = std::thread::Builder::new()
            .name(format!("tea-mcp-stderr-{}", self.config.name))
            .spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    let mut tail = tail.lock().expect("MCP stderr poisoned");
                    if tail.len() == 32 {
                        tail.pop_front();
                    }
                    tail.push_back(tea_core::tool::truncate_middle(&line, 400));
                }
            })
            .ok();
        let (connection, reader) =
            match Connection::start(&self.config.name, Box::new(stdout), Box::new(stdin)) {
                Ok(started) => started,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    self.fail(format!("could not start the reader: {error}"));
                    return;
                }
            };
        {
            let mut state = self.state.lock().expect("MCP server poisoned");
            if state.shut_down {
                drop(state);
                stop(Running {
                    child,
                    connection,
                    reader: Some(reader),
                    stderr,
                });
                return;
            }
            state.running = Some(Running {
                child,
                connection: Arc::clone(&connection),
                reader: Some(reader),
                stderr,
            });
        }
        match initialize(&self.config, &connection).and_then(|()| list_tools(&self.config, &connection)) {
            Ok(tools) => {
                let mut state = self.state.lock().expect("MCP server poisoned");
                if state.running.is_some() {
                    state.status = ServerStatus::Ready { tools: tools.len() };
                    state.tools = tools;
                }
                drop(state);
                self.changed.notify_all();
            }
            Err(error) => {
                let diagnostics = connection.diagnostics();
                match diagnostics.last() {
                    Some(last) => self.fail(format!("{error} (last protocol note: {last})")),
                    None => self.fail(error),
                }
            }
        }
    }

    /// Record an unexpected exit without blocking.
    fn observe_exit(&self, state: &mut ServerState) {
        let exited = state.running.as_mut().and_then(|running| {
            match running.child.try_wait() {
                Ok(Some(status)) => Some(format!("exited with {status}")),
                Ok(None) => running.connection.closed(),
                Err(error) => Some(format!("could not be observed: {error}")),
            }
        });
        if let Some(reason) = exited {
            if let Some(running) = state.running.take() {
                stop(running);
            }
            state.tools.clear();
            state.status = ServerStatus::Exited(reason);
        }
    }

    fn shutdown(&self) {
        let running = {
            let mut state = self.state.lock().expect("MCP server poisoned");
            state.shut_down = true;
            state.tools.clear();
            if matches!(state.status, ServerStatus::Starting | ServerStatus::Ready { .. }) {
                state.status = ServerStatus::Exited("shut down".into());
            }
            state.running.take()
        };
        self.changed.notify_all();
        if let Some(running) = running {
            stop(running);
        }
    }
}

/// Close input, wait briefly, then kill and reap. Reader threads end at EOF.
fn stop(mut running: Running) {
    running.connection.shutdown("server is shutting down");
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        match running.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = running.child.kill();
                let _ = running.child.wait();
                break;
            }
        }
    }
    if let Some(reader) = running.reader.take() {
        let _ = reader.join();
    }
    if let Some(stderr) = running.stderr.take() {
        let _ = stderr.join();
    }
}

fn initialize(config: &McpServerConfig, connection: &Connection) -> Result<(), String> {
    let response = connection
        .request(
            "initialize",
            JsonValue::object([
                ("protocolVersion", JsonValue::from(PROTOCOL_VERSION)),
                (
                    "capabilities",
                    JsonValue::object(Vec::<(&str, JsonValue)>::new()),
                ),
                (
                    "clientInfo",
                    JsonValue::object([
                        ("name", JsonValue::from("tea")),
                        ("version", JsonValue::from(env!("CARGO_PKG_VERSION"))),
                    ]),
                ),
            ]),
        )
        .map_err(|error| format!("initialize failed: {error}"))?;
    let result = response
        .wait(config.startup_timeout)
        .map_err(|error| format!("initialize failed: {error}"))?;
    if result
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("tools"))
        .is_none()
    {
        return Err("server does not offer tools".into());
    }
    connection
        .notify(
            "notifications/initialized",
            JsonValue::object(Vec::<(&str, JsonValue)>::new()),
        )
        .map_err(|error| format!("initialized notification failed: {error}"))
}

/// Largest number of tools accepted from one server.
const MAX_TOOLS: usize = 512;

fn list_tools(
    config: &McpServerConfig,
    connection: &Arc<Connection>,
) -> Result<Vec<Arc<dyn AgentTool>>, String> {
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    let mut cursor: Option<String> = None;
    for _page in 0..64 {
        let params = match &cursor {
            Some(cursor) => JsonValue::object([("cursor", JsonValue::from(cursor.as_str()))]),
            None => JsonValue::object(Vec::<(&str, JsonValue)>::new()),
        };
        let result = connection
            .request("tools/list", params)
            .and_then(|response| response.wait(config.startup_timeout))
            .map_err(|error| format!("tools/list failed: {error}"))?;
        for tool in result
            .get("tools")
            .and_then(JsonValue::as_array)
            .unwrap_or_default()
        {
            let Some(remote) = tool.get("name").and_then(JsonValue::as_str) else {
                continue;
            };
            let name = tool_name(&config.name, remote);
            if !names.insert(name.clone()) || tools.len() == MAX_TOOLS {
                continue;
            }
            let description = tool
                .get("description")
                .and_then(JsonValue::as_str)
                .or_else(|| tool.get("title").and_then(JsonValue::as_str))
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("MCP tool {remote} from server {}.", config.name));
            tools.push(Arc::new(McpTool {
                name,
                remote: remote.to_owned(),
                server: config.name.clone(),
                description,
                schema: normalize_input_schema(tool.get("inputSchema")),
                exposure: config.exposure,
                timeout: config.call_timeout,
                connection: Arc::clone(connection),
            }));
        }
        cursor = result
            .get("nextCursor")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return Ok(tools);
        }
    }
    Ok(tools)
}

/// `mcp__<server>__<tool>`, restricted to `[A-Za-z0-9_-]` and 64 bytes so it
/// is a valid tool name for every provider.
pub(crate) fn tool_name(server: &str, tool: &str) -> String {
    let sanitize = |value: &str| {
        value
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let mut name = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    name.truncate(64);
    name
}

/// An MCP tool exposed as an ordinary trusted tool.
struct McpTool {
    name: String,
    remote: String,
    server: String,
    description: String,
    schema: JsonValue,
    exposure: ToolExposure,
    timeout: Duration,
    connection: Arc<Connection>,
}

impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> &JsonValue {
        &self.schema
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments = JsonValue::parse(call.arguments.as_str()).map_err(|error| {
                ToolError::InvalidArguments {
                    tool: self.name.clone(),
                    message: error.to_string(),
                }
            })?;
            let response = self
                .connection
                .request(
                    "tools/call",
                    JsonValue::object([
                        ("name", JsonValue::from(self.remote.as_str())),
                        ("arguments", arguments),
                    ]),
                )
                .map_err(|error| self.unavailable(error))?;
            let id = response.id();
            enum Outcome {
                Answer(Result<JsonValue, RpcError>),
                Cancelled,
                TimedOut,
            }
            let outcome = smol::future::or(
                async { Outcome::Answer(response.await) },
                smol::future::or(
                    async {
                        context.cancellation.cancelled().await;
                        Outcome::Cancelled
                    },
                    async {
                        smol::Timer::after(self.timeout).await;
                        Outcome::TimedOut
                    },
                ),
            )
            .await;
            let result = match outcome {
                Outcome::Answer(result) => result.map_err(|error| self.unavailable(error))?,
                Outcome::Cancelled => {
                    self.connection.cancel(id, "cancelled by tea");
                    return Err(ToolError::Cancelled {
                        tool: self.name.clone(),
                    });
                }
                Outcome::TimedOut => {
                    self.connection.cancel(id, "timed out");
                    return Err(ToolError::Execution {
                        tool: self.name.clone(),
                        message: format!(
                            "MCP server {} did not answer within {}s",
                            self.server,
                            self.timeout.as_secs()
                        ),
                    });
                }
            };
            Ok(map_call_result(call, &self.server, &self.remote, &result))
        })
    }
}

impl McpTool {
    fn unavailable(&self, error: RpcError) -> ToolError {
        ToolError::Execution {
            tool: self.name.clone(),
            message: format!("MCP server {} failed the call: {error}", self.server),
        }
    }
}

/// Map a `CallToolResult` to a tool result. Text is kept; content tea cannot
/// present (images, audio, binary resources) is named with its type and size
/// instead of being dropped or inlined as base64.
pub(crate) fn map_call_result(
    call: ToolCall,
    server: &str,
    tool: &str,
    result: &JsonValue,
) -> AgentToolResult {
    let mut parts = Vec::new();
    let mut unsupported = Vec::new();
    for item in result
        .get("content")
        .and_then(JsonValue::as_array)
        .unwrap_or_default()
    {
        let kind = item.get("type").and_then(JsonValue::as_str).unwrap_or("unknown");
        match kind {
            "text" => parts.push(
                item.get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            "image" | "audio" => {
                let mime = item.get("mimeType").and_then(JsonValue::as_str).unwrap_or("unknown");
                let bytes = item
                    .get("data")
                    .and_then(JsonValue::as_str)
                    .map_or(0, str::len);
                parts.push(format!(
                    "[unsupported {kind} content omitted: {mime}, {bytes} base64 bytes]"
                ));
                unsupported.push(JsonValue::from(kind));
            }
            "resource" => {
                let resource = item.get("resource");
                let uri = resource
                    .and_then(|resource| resource.get("uri"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown");
                match resource
                    .and_then(|resource| resource.get("text"))
                    .and_then(JsonValue::as_str)
                {
                    Some(text) => parts.push(format!("[resource {uri}]\n{text}")),
                    None => {
                        let mime = resource
                            .and_then(|resource| resource.get("mimeType"))
                            .and_then(JsonValue::as_str)
                            .unwrap_or("unknown");
                        let bytes = resource
                            .and_then(|resource| resource.get("blob"))
                            .and_then(JsonValue::as_str)
                            .map_or(0, str::len);
                        parts.push(format!(
                            "[unsupported binary resource omitted: {uri}, {mime}, {bytes} base64 bytes]"
                        ));
                        unsupported.push(JsonValue::from("resource"));
                    }
                }
            }
            "resource_link" => {
                let uri = item.get("uri").and_then(JsonValue::as_str).unwrap_or("unknown");
                let name = item.get("name").and_then(JsonValue::as_str).unwrap_or(uri);
                parts.push(format!("[resource link: {name} <{uri}>]"));
            }
            other => {
                parts.push(format!("[unsupported content type {other:?} omitted]"));
                unsupported.push(JsonValue::from(other));
            }
        }
    }
    let structured = result.get("structuredContent");
    if parts.is_empty() {
        if let Some(structured) = structured {
            parts.push(structured.to_json_string().unwrap_or_default());
        }
    }
    let is_error = result.get("isError").and_then(JsonValue::as_bool) == Some(true);
    let mut details = vec![
        ("server", JsonValue::from(server)),
        ("tool", JsonValue::from(tool)),
    ];
    if let Some(structured) = structured {
        details.push(("structuredContent", structured.clone()));
    }
    if !unsupported.is_empty() {
        details.push(("unsupportedContent", JsonValue::Array(unsupported)));
    }
    AgentToolResult {
        tool_call_id: call.id,
        content: parts.join("\n"),
        details: JsonValue::object(details)
            .to_json_string()
            .ok()
            .map(SerializedJson::new),
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error,
        failure: is_error.then(tea_core::tool::ToolFailure::recoverable),
    }
}

/// Keywords the core argument validator understands.
const SCHEMA_KEYWORDS: [&str; 26] = [
    "additionalProperties",
    "allOf",
    "anyOf",
    "const",
    "default",
    "deprecated",
    "description",
    "enum",
    "examples",
    "exclusiveMaximum",
    "exclusiveMinimum",
    "items",
    "maxItems",
    "maxLength",
    "maximum",
    "minItems",
    "minLength",
    "minimum",
    "not",
    "oneOf",
    "properties",
    "required",
    "title",
    "type",
    "uniqueItems",
    "$comment",
];

/// Reduce an MCP input schema to the vocabulary tea validates.
///
/// Local `$ref`s into `$defs`/`definitions` are inlined (cycles and other refs
/// become unconstrained), and keywords tea does not enforce — `format`,
/// `pattern`, and the like — are dropped. The server still validates its own
/// input, so this only loosens tea's early check; it never makes a valid call
/// invalid.
pub(crate) fn normalize_input_schema(schema: Option<&JsonValue>) -> JsonValue {
    let root = schema.cloned().unwrap_or(JsonValue::Null);
    let mut normalized = normalize(&root, &root, 0);
    if let Some(object) = normalized.as_object_mut() {
        object.insert("type".into(), JsonValue::from("object"));
        if !object.contains_key("properties") {
            object.insert(
                "properties".into(),
                JsonValue::object(Vec::<(&str, JsonValue)>::new()),
            );
        }
    }
    normalized
}

fn normalize(schema: &JsonValue, root: &JsonValue, depth: usize) -> JsonValue {
    let empty = || JsonValue::object(Vec::<(&str, JsonValue)>::new());
    if depth > 16 {
        return empty();
    }
    let object = match schema {
        JsonValue::Object(object) => object,
        JsonValue::Bool(false) => return JsonValue::object([("not", empty())]),
        _ => return empty(),
    };
    if let Some(reference) = object.get("$ref").and_then(JsonValue::as_str) {
        let target = reference
            .strip_prefix("#/$defs/")
            .map(|name| ("$defs", name))
            .or_else(|| reference.strip_prefix("#/definitions/").map(|name| ("definitions", name)))
            .and_then(|(section, name)| root.get(section).and_then(|section| section.get(name)));
        return match target {
            Some(target) => {
                let mut resolved = normalize(target, root, depth + 1);
                if let (Some(resolved), Some(description)) = (
                    resolved.as_object_mut(),
                    object.get("description").and_then(JsonValue::as_str),
                ) {
                    resolved.insert("description".into(), JsonValue::from(description));
                }
                resolved
            }
            None => empty(),
        };
    }
    let mut output = BTreeMap::new();
    for (key, value) in object {
        if !SCHEMA_KEYWORDS.contains(&key.as_str()) {
            continue;
        }
        let normalized = match key.as_str() {
            "properties" => match value.as_object() {
                Some(properties) => JsonValue::Object(
                    properties
                        .iter()
                        .map(|(name, property)| (name.clone(), normalize(property, root, depth + 1)))
                        .collect(),
                ),
                None => continue,
            },
            "items" => match value {
                JsonValue::Object(_) | JsonValue::Bool(_) => normalize(value, root, depth + 1),
                _ => continue,
            },
            "additionalProperties" => match value {
                JsonValue::Bool(_) => value.clone(),
                JsonValue::Object(_) => normalize(value, root, depth + 1),
                _ => continue,
            },
            "not" => normalize(value, root, depth + 1),
            "allOf" | "anyOf" | "oneOf" => match value.as_array() {
                Some(variants) => JsonValue::Array(
                    variants
                        .iter()
                        .map(|variant| normalize(variant, root, depth + 1))
                        .collect(),
                ),
                None => continue,
            },
            "type" => match value {
                JsonValue::String(name) if supported_type(name) => value.clone(),
                JsonValue::Array(names)
                    if !names.is_empty()
                        && names
                            .iter()
                            .all(|name| name.as_str().is_some_and(supported_type)) =>
                {
                    value.clone()
                }
                _ => continue,
            },
            "required" => match value.as_array() {
                Some(names) => {
                    let mut unique = Vec::new();
                    for name in names.iter().filter_map(JsonValue::as_str) {
                        if !unique.contains(&name) {
                            unique.push(name);
                        }
                    }
                    JsonValue::Array(unique.into_iter().map(JsonValue::from).collect())
                }
                None => continue,
            },
            "enum" if value.as_array().is_none() => continue,
            "minItems" | "maxItems" | "minLength" | "maxLength" if value.as_u64().is_none() => {
                continue
            }
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
                if value.as_f64().is_none() =>
            {
                continue
            }
            "uniqueItems" if value.as_bool().is_none() => continue,
            _ => value.clone(),
        };
        output.insert(key.clone(), normalized);
    }
    JsonValue::Object(output)
}

fn supported_type(name: &str) -> bool {
    matches!(
        name,
        "null" | "boolean" | "integer" | "number" | "string" | "array" | "object"
    )
}

#[cfg(test)]
pub(crate) mod tests;
