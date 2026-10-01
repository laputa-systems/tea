//! Newline-delimited JSON-RPC 2.0 over one MCP stdio connection.
//!
//! One reader thread owns the server's stdout. It completes pending requests,
//! answers the few server-initiated requests a tool-only client must handle
//! (`ping`; everything else is "method not found"), and records bounded
//! diagnostics. No executor is involved: request futures wait on a slot
//! woken by the reader thread, and blocking waits use a condition variable.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};
use tea_protocol::JsonValue;

/// Largest accepted message line.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Diagnostic lines retained per connection.
const DIAGNOSTIC_LINES: usize = 32;

/// A failed request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpcError {
    /// The server answered with a JSON-RPC error.
    Remote { code: i64, message: String },
    /// The connection is closed.
    Closed(String),
    /// No answer arrived in time.
    Timeout,
    /// The message could not be written.
    Io(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote { code, message } => write!(formatter, "MCP error {code}: {message}"),
            Self::Closed(reason) => write!(formatter, "MCP connection closed: {reason}"),
            Self::Timeout => formatter.write_str("MCP request timed out"),
            Self::Io(message) => write!(formatter, "MCP write failed: {message}"),
        }
    }
}

#[derive(Default)]
struct Slot {
    state: Mutex<(Option<Result<JsonValue, RpcError>>, Option<Waker>)>,
    ready: Condvar,
}

impl Slot {
    fn complete(&self, result: Result<JsonValue, RpcError>) {
        let waker = {
            let mut state = self.state.lock().expect("MCP slot poisoned");
            if state.0.is_some() {
                return;
            }
            state.0 = Some(result);
            state.1.take()
        };
        self.ready.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

#[derive(Default)]
struct State {
    next_id: u64,
    pending: BTreeMap<u64, Arc<Slot>>,
    closed: Option<String>,
    tools_changed: bool,
    diagnostics: VecDeque<String>,
}

struct Shared {
    state: Mutex<State>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
}

impl Shared {
    fn write(&self, message: &JsonValue) -> Result<(), RpcError> {
        let mut line = message
            .to_json_string()
            .map_err(|error| RpcError::Io(error.to_string()))?;
        line.push('\n');
        let mut writer = self.writer.lock().expect("MCP writer poisoned");
        let writer = writer
            .as_mut()
            .ok_or_else(|| RpcError::Closed("stdin closed".into()))?;
        writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|error| RpcError::Io(error.to_string()))
    }

    fn diagnostic(&self, line: String) {
        let mut state = self.state.lock().expect("MCP state poisoned");
        if state.diagnostics.len() == DIAGNOSTIC_LINES {
            state.diagnostics.pop_front();
        }
        state.diagnostics.push_back(line);
    }

    fn close(&self, reason: String) {
        let pending = {
            let mut state = self.state.lock().expect("MCP state poisoned");
            if state.closed.is_none() {
                state.closed = Some(reason.clone());
            }
            std::mem::take(&mut state.pending)
        };
        for slot in pending.values() {
            slot.complete(Err(RpcError::Closed(reason.clone())));
        }
    }
}

/// One JSON-RPC connection.
pub(crate) struct Connection {
    shared: Arc<Shared>,
}

/// A pending response.
pub(crate) struct Response {
    id: u64,
    slot: Arc<Slot>,
}

impl Response {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Block the calling thread until the answer arrives or `timeout` passes.
    pub(crate) fn wait(&self, timeout: Duration) -> Result<JsonValue, RpcError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.slot.state.lock().expect("MCP slot poisoned");
        loop {
            if let Some(result) = state.0.take() {
                return result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RpcError::Timeout);
            }
            state = self
                .slot
                .ready
                .wait_timeout(state, remaining)
                .expect("MCP slot poisoned")
                .0;
        }
    }
}

impl Future for Response {
    type Output = Result<JsonValue, RpcError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.slot.state.lock().expect("MCP slot poisoned");
        match state.0.take() {
            Some(result) => Poll::Ready(result),
            None => {
                state.1 = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Connection {
    /// Start a connection over a server's stdout and stdin. The returned
    /// thread ends when the reader reaches end of file.
    pub(crate) fn start(
        name: &str,
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
    ) -> std::io::Result<(Arc<Self>, std::thread::JoinHandle<()>)> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            writer: Mutex::new(Some(writer)),
        });
        let reader_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name(format!("tea-mcp-{name}"))
            .spawn(move || read_loop(reader, &reader_shared))?;
        Ok((Arc::new(Self { shared }), thread))
    }

    /// Send a request.
    pub(crate) fn request(&self, method: &str, params: JsonValue) -> Result<Response, RpcError> {
        let (id, slot) = {
            let mut state = self.shared.state.lock().expect("MCP state poisoned");
            if let Some(reason) = &state.closed {
                return Err(RpcError::Closed(reason.clone()));
            }
            state.next_id += 1;
            let id = state.next_id;
            let slot = Arc::new(Slot::default());
            state.pending.insert(id, Arc::clone(&slot));
            (id, slot)
        };
        let message = JsonValue::object([
            ("jsonrpc", JsonValue::from("2.0")),
            ("id", JsonValue::from(id)),
            ("method", JsonValue::from(method)),
            ("params", params),
        ]);
        if let Err(error) = self.shared.write(&message) {
            self.shared
                .state
                .lock()
                .expect("MCP state poisoned")
                .pending
                .remove(&id);
            return Err(error);
        }
        Ok(Response { id, slot })
    }

    /// Send a notification.
    pub(crate) fn notify(&self, method: &str, params: JsonValue) -> Result<(), RpcError> {
        self.shared.write(&JsonValue::object([
            ("jsonrpc", JsonValue::from("2.0")),
            ("method", JsonValue::from(method)),
            ("params", params),
        ]))
    }

    /// Abandon a request: forget its slot and tell the server.
    pub(crate) fn cancel(&self, id: u64, reason: &str) {
        let removed = self
            .shared
            .state
            .lock()
            .expect("MCP state poisoned")
            .pending
            .remove(&id);
        if removed.is_some() {
            let _ = self.notify(
                "notifications/cancelled",
                JsonValue::object([
                    ("requestId", JsonValue::from(id)),
                    ("reason", JsonValue::from(reason)),
                ]),
            );
        }
    }

    /// Why the connection closed, if it has.
    pub(crate) fn closed(&self) -> Option<String> {
        self.shared
            .state
            .lock()
            .expect("MCP state poisoned")
            .closed
            .clone()
    }

    /// Whether the server announced a changed tool list since the last call.
    pub(crate) fn take_tools_changed(&self) -> bool {
        std::mem::take(
            &mut self
                .shared
                .state
                .lock()
                .expect("MCP state poisoned")
                .tools_changed,
        )
    }

    /// Recent protocol diagnostics.
    pub(crate) fn diagnostics(&self) -> Vec<String> {
        self.shared
            .state
            .lock()
            .expect("MCP state poisoned")
            .diagnostics
            .iter()
            .cloned()
            .collect()
    }

    /// Close stdin so a well-behaved server exits, and fail pending requests.
    pub(crate) fn shutdown(&self, reason: &str) {
        self.shared
            .writer
            .lock()
            .expect("MCP writer poisoned")
            .take();
        self.shared.close(reason.into());
    }
}

fn read_loop(reader: Box<dyn Read + Send>, shared: &Shared) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = match (&mut reader)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(read) => read,
            Err(error) => {
                shared.close(format!("read failed: {error}"));
                return;
            }
        };
        if read == 0 {
            shared.close("server closed its output".into());
            return;
        }
        if line.len() > MAX_LINE_BYTES {
            shared.close("server sent an oversized message".into());
            return;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match JsonValue::parse(text) {
            Ok(message) => dispatch(shared, message),
            Err(_) => shared.diagnostic(format!(
                "ignored non-JSON output: {}",
                tea_core::tool::truncate_middle(text, 200)
            )),
        }
    }
}

fn dispatch(shared: &Shared, message: JsonValue) {
    let method = message.get("method").and_then(JsonValue::as_str);
    let id = message.get("id");
    match (method, id) {
        // A response to one of our requests.
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else {
                shared.diagnostic("ignored response with a non-numeric id".into());
                return;
            };
            let slot = shared
                .state
                .lock()
                .expect("MCP state poisoned")
                .pending
                .remove(&id);
            let Some(slot) = slot else {
                return;
            };
            let result = match (message.get("result"), message.get("error")) {
                (_, Some(error)) => Err(RpcError::Remote {
                    code: error
                        .get("code")
                        .and_then(|code| code.as_f64())
                        .map_or(-32603, |code| code as i64),
                    message: error
                        .get("message")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("unknown error")
                        .to_owned(),
                }),
                (Some(result), None) => Ok(result.clone()),
                (None, None) => Err(RpcError::Remote {
                    code: -32603,
                    message: "response has neither result nor error".into(),
                }),
            };
            slot.complete(result);
        }
        // A server-initiated request.
        (Some(method), Some(id)) => {
            let response = if method == "ping" {
                JsonValue::object([
                    ("jsonrpc", JsonValue::from("2.0")),
                    ("id", id.clone()),
                    ("result", JsonValue::object(Vec::<(&str, JsonValue)>::new())),
                ])
            } else {
                JsonValue::object([
                    ("jsonrpc", JsonValue::from("2.0")),
                    ("id", id.clone()),
                    (
                        "error",
                        JsonValue::object([
                            ("code", JsonValue::from(-32601_i64)),
                            (
                                "message",
                                JsonValue::from(format!("tea does not support {method}")),
                            ),
                        ]),
                    ),
                ])
            };
            let _ = shared.write(&response);
        }
        // A notification.
        (Some(method), None) => {
            if method == "notifications/tools/list_changed" {
                shared.state.lock().expect("MCP state poisoned").tools_changed = true;
            } else if method == "notifications/message" {
                let data = message
                    .get("params")
                    .and_then(|params| params.get("data"))
                    .and_then(|data| data.to_json_string().ok())
                    .unwrap_or_default();
                shared.diagnostic(format!(
                    "server log: {}",
                    tea_core::tool::truncate_middle(&data, 200)
                ));
            }
        }
        (None, None) => shared.diagnostic("ignored message without method or id".into()),
    }
}
