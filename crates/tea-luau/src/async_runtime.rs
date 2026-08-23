//! Caller-driven asynchronous Luau coroutines.
//!
//! The workspace intentionally does not enable mlua's `async` feature: the core
//! must not select an executor.  This module therefore uses the ordinary Luau
//! coroutine protocol.  [`install_await`] injects a small `await` helper.  A
//! script yields a request table, the embedding host starts and polls its own
//! future, and [`LuauTask`] resumes the coroutine with the result.
//!
//! Host futures are owned by the task while pending.  Cancellation drops that
//! future before the task settles, so a well-behaved capability adapter cannot
//! leave work owned by this boundary.  Capability adapters should still use the
//! supplied [`CancellationToken`] to cancel any work they started elsewhere.

use mlua::{
    thread::ThreadStatus, Error as LuaError, Function, IntoLuaMulti, Lua, MultiValue, Nil, Table,
    Thread,
};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tea_core::scheduler::{CancellationToken, CancellationWait};

/// Global Luau function installed by [`install_await`].
pub const AWAIT_GLOBAL: &str = "await";

const AWAIT_FACTORY_GLOBAL: &str = "__pi_make_await_request";
const AWAIT_MARKER_FIELD: &str = "__pi_await_request";
const AWAIT_CAPABILITY_FIELD: &str = "capability";
const AWAIT_ARGUMENTS_FIELD: &str = "arguments_json";

/// A capability request yielded by a Luau coroutine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostRequest {
    /// The explicitly granted host capability to invoke.
    pub capability: String,
    /// JSON arguments for the capability operation.
    pub arguments_json: String,
}

impl HostRequest {
    fn validate(self) -> Result<Self, AsyncRuntimeError> {
        if self.capability.trim().is_empty() {
            return Err(AsyncRuntimeError::Protocol {
                message: "await request capability must not be empty".to_owned(),
            });
        }
        if self.arguments_json.trim().is_empty() {
            return Err(AsyncRuntimeError::Protocol {
                message: format!(
                    "await request for capability {:?} has empty arguments_json",
                    self.capability
                ),
            });
        }
        Ok(self)
    }
}

/// A typed failure returned by a host capability adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCallError {
    /// A redacted diagnostic suitable for returning to the embedding host.
    pub message: String,
}

impl HostCallError {
    /// Construct a host capability failure.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HostCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HostCallError {}

/// The future type owned by a pending host capability call.
pub type HostFuture = Pin<Box<dyn Future<Output = Result<String, HostCallError>> + Send + 'static>>;

/// Host capability adapter used by [`LuauAsyncRuntime`].
pub trait HostAwaiter: Send + Sync {
    /// Start one explicitly authorized operation.
    fn start(&self, request: HostRequest, cancellation: CancellationToken) -> HostFuture;
}

impl<F, Fut> HostAwaiter for F
where
    F: Fn(HostRequest, CancellationToken) -> Fut + Send + Sync,
    Fut: Future<Output = Result<String, HostCallError>> + Send + 'static,
{
    fn start(&self, request: HostRequest, cancellation: CancellationToken) -> HostFuture {
        Box::pin(self(request, cancellation))
    }
}

/// A typed failure while driving a Luau coroutine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AsyncRuntimeError {
    /// The task was cancelled before its pending host operation settled.
    Cancelled,
    /// A host capability failed.
    Host {
        /// Capability that failed.
        capability: String,
        /// Redacted host diagnostic.
        message: String,
    },
    /// Luau rejected a resume or raised an exception.
    Lua {
        /// Host-safe Luau diagnostic.
        message: String,
    },
    /// The script yielded a value outside the await protocol.
    Protocol {
        /// Searchable explanation of the protocol violation.
        message: String,
    },
}

impl fmt::Display for AsyncRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Luau task was cancelled"),
            Self::Host {
                capability,
                message,
            } => write!(
                formatter,
                "Luau capability {capability:?} failed: {message}"
            ),
            Self::Lua { message } => write!(formatter, "Luau task failed: {message}"),
            Self::Protocol { message } => {
                write!(formatter, "invalid Luau await protocol: {message}")
            }
        }
    }
}

impl Error for AsyncRuntimeError {}

/// Install the `await(capability, arguments_json)` helper in a Luau state.
///
/// The helper is intentionally only a request constructor and coroutine yield:
/// it cannot invoke a capability or acquire ambient authority.  Call this
/// before loading a sandboxed bundle that calls `await`.  The bundle receives
/// the host result as `(true, value, nil)` or is terminated by the typed host
/// error returned from [`LuauTask`].
pub fn install_await(lua: &Lua) -> mlua::Result<()> {
    let factory = lua.create_function(|lua, (capability, arguments_json): (String, String)| {
        if capability.trim().is_empty() {
            return Err(LuaError::RuntimeError(
                "await capability must not be empty".to_owned(),
            ));
        }
        if arguments_json.trim().is_empty() {
            return Err(LuaError::RuntimeError(
                "await arguments_json must not be empty".to_owned(),
            ));
        }
        let request = lua.create_table()?;
        request.set(AWAIT_MARKER_FIELD, true)?;
        request.set(AWAIT_CAPABILITY_FIELD, capability)?;
        request.set(AWAIT_ARGUMENTS_FIELD, arguments_json)?;
        Ok(request)
    })?;
    lua.globals().set(AWAIT_FACTORY_GLOBAL, factory)?;

    let await_function: Function = lua
        .load(
            r#"
                return function(capability, arguments_json)
                    local request = __pi_make_await_request(capability, arguments_json)
                    local ok, value, message = coroutine.yield(request)
                    if not ok then
                        error(message or "host capability failed", 0)
                    end
                    return value
                end
            "#,
        )
        .eval()?;
    lua.globals().set(AWAIT_GLOBAL, await_function)
}

/// A caller-owned runtime that creates Luau tasks but does not run an executor.
#[derive(Clone)]
pub struct LuauAsyncRuntime {
    lua: Lua,
    host: Arc<dyn HostAwaiter>,
}

impl LuauAsyncRuntime {
    /// Construct a runtime over an existing Luau state and host capability set.
    pub fn new(lua: &Lua, host: Arc<dyn HostAwaiter>) -> Self {
        Self {
            lua: lua.clone(),
            host,
        }
    }

    /// Start an entry function.  The returned task is driven by the caller's
    /// executor through its ordinary [`Future`] implementation.
    pub fn start<A>(&self, function: Function, arguments: A) -> Result<LuauTask, AsyncRuntimeError>
    where
        A: IntoLuaMulti,
    {
        self.start_with_cancellation(function, arguments, CancellationToken::new())
    }

    /// Start an entry function in an embedding-owned cancellation scope.
    pub fn start_with_cancellation<A>(
        &self,
        function: Function,
        arguments: A,
        cancellation: CancellationToken,
    ) -> Result<LuauTask, AsyncRuntimeError>
    where
        A: IntoLuaMulti,
    {
        let arguments = arguments.into_lua_multi(&self.lua).map_err(lua_error)?;
        let thread = self.lua.create_thread(function).map_err(lua_error)?;
        Ok(LuauTask {
            thread,
            host: Arc::clone(&self.host),
            cancellation,
            initial_arguments: Some(arguments),
            pending: None,
            terminal: false,
        })
    }
}

struct PendingCall {
    request: HostRequest,
    future: HostFuture,
    cancellation: CancellationWait,
}

/// A Luau coroutine plus at most one pending host capability future.
pub struct LuauTask {
    thread: Thread,
    host: Arc<dyn HostAwaiter>,
    cancellation: CancellationToken,
    initial_arguments: Option<MultiValue>,
    pending: Option<PendingCall>,
    terminal: bool,
}

impl LuauTask {
    /// Return the cancellation scope shared with host capability operations.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Request cancellation.  The next poll drops the pending host future and
    /// settles with [`AsyncRuntimeError::Cancelled`].
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}

impl Future for LuauTask {
    type Output = Result<MultiValue, AsyncRuntimeError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let task = self.get_mut();
        if task.terminal {
            return Poll::Ready(Err(AsyncRuntimeError::Protocol {
                message: "Luau task was polled after completion".to_owned(),
            }));
        }

        loop {
            if task.cancellation.is_cancelled() {
                task.pending.take();
                task.terminal = true;
                return Poll::Ready(Err(AsyncRuntimeError::Cancelled));
            }

            let yielded = if let Some(mut pending) = task.pending.take() {
                if Pin::new(&mut pending.cancellation).poll(context).is_ready() {
                    task.terminal = true;
                    return Poll::Ready(Err(AsyncRuntimeError::Cancelled));
                }
                match pending.future.as_mut().poll(context) {
                    Poll::Pending => {
                        task.pending = Some(pending);
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(value)) => {
                        if task.cancellation.is_cancelled() {
                            task.terminal = true;
                            return Poll::Ready(Err(AsyncRuntimeError::Cancelled));
                        }
                        task.thread.resume::<MultiValue>((true, value, Nil))
                    }
                    Poll::Ready(Err(error)) => {
                        task.terminal = true;
                        return Poll::Ready(Err(AsyncRuntimeError::Host {
                            capability: pending.request.capability,
                            message: error.message,
                        }));
                    }
                }
            } else {
                task.thread
                    .resume::<MultiValue>(task.initial_arguments.take().unwrap_or_default())
            };

            let values = match yielded.map_err(lua_error) {
                Ok(values) => values,
                Err(error) => {
                    task.terminal = true;
                    return Poll::Ready(Err(error));
                }
            };

            if task.cancellation.is_cancelled() {
                task.terminal = true;
                return Poll::Ready(Err(AsyncRuntimeError::Cancelled));
            }
            if task.thread.status() != ThreadStatus::Resumable {
                task.terminal = true;
                return Poll::Ready(Ok(values));
            }

            let request = match parse_request(values) {
                Ok(request) => request,
                Err(error) => {
                    task.terminal = true;
                    return Poll::Ready(Err(error));
                }
            };
            let future = task.host.start(request.clone(), task.cancellation.clone());
            task.pending = Some(PendingCall {
                request,
                future,
                cancellation: task.cancellation.cancelled(),
            });
            // An immediately-ready host future is allowed to make progress in
            // this poll.  A pending future returns above on the next iteration.
        }
    }
}

fn parse_request(values: MultiValue) -> Result<HostRequest, AsyncRuntimeError> {
    if values.len() != 1 {
        return Err(AsyncRuntimeError::Protocol {
            message: format!(
                "await must yield exactly one request table, got {} values",
                values.len()
            ),
        });
    }
    let value = values.into_iter().next().expect("length checked above");
    let ValueTable(table) = ValueTable::try_from(value)?;
    let marker = table
        .get::<Option<bool>>(AWAIT_MARKER_FIELD)
        .map_err(lua_error)?;
    if marker != Some(true) {
        return Err(AsyncRuntimeError::Protocol {
            message: "yielded table is not an await request".to_owned(),
        });
    }
    let request = HostRequest {
        capability: table
            .get::<String>(AWAIT_CAPABILITY_FIELD)
            .map_err(lua_error)?,
        arguments_json: table
            .get::<String>(AWAIT_ARGUMENTS_FIELD)
            .map_err(lua_error)?,
    };
    request.validate()
}

struct ValueTable(Table);

impl TryFrom<mlua::Value> for ValueTable {
    type Error = AsyncRuntimeError;

    fn try_from(value: mlua::Value) -> Result<Self, Self::Error> {
        match value {
            mlua::Value::Table(table) => Ok(Self(table)),
            other => Err(AsyncRuntimeError::Protocol {
                message: format!(
                    "await yielded {}, expected a request table",
                    other.type_name()
                ),
            }),
        }
    }
}

fn lua_error(error: LuaError) -> AsyncRuntimeError {
    AsyncRuntimeError::Lua {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{install_await, AsyncRuntimeError, HostCallError, HostRequest, LuauAsyncRuntime};
    use mlua::{Function, Lua, StdLib};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};
    use tea_core::scheduler::CancellationToken;

    fn lua() -> Lua {
        Lua::new_with(
            StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING,
            mlua::LuaOptions::new(),
        )
        .expect("test Lua state")
    }

    fn poll_once<T>(future: &mut T) -> Poll<T::Output>
    where
        T: Future + Unpin,
    {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        Pin::new(future).poll(&mut context)
    }

    #[test]
    fn await_request_round_trips_through_host_future() {
        let lua = lua();
        install_await(&lua).expect("await helper");
        let function: Function = lua
            .load("return function() return await('rs-agent', '{}') end")
            .eval()
            .expect("entry function");
        let seen = Arc::new(std::sync::Mutex::new(None::<HostRequest>));
        let seen_by_host = Arc::clone(&seen);
        let host = Arc::new(move |request: HostRequest, _: CancellationToken| {
            *seen_by_host.lock().expect("seen lock") = Some(request);
            std::future::ready(Ok::<_, HostCallError>(r#"{"ok":true}"#.to_owned()))
        });
        let runtime = LuauAsyncRuntime::new(&lua, host);
        let mut task = runtime.start(function, ()).expect("start task");
        let output = match poll_once(&mut task) {
            Poll::Ready(Ok(values)) => values,
            other => panic!("unexpected task state: {other:?}"),
        };
        assert_eq!(output.len(), 1);
        assert_eq!(
            seen.lock().expect("seen lock").as_ref().unwrap().capability,
            "rs-agent"
        );
        assert_eq!(
            seen.lock()
                .expect("seen lock")
                .as_ref()
                .unwrap()
                .arguments_json,
            "{}"
        );
    }

    struct PendingUntilDropped(Arc<AtomicBool>);

    impl Future for PendingUntilDropped {
        type Output = Result<String, HostCallError>;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingUntilDropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn cancellation_drops_pending_host_future_before_settling() {
        let lua = lua();
        install_await(&lua).expect("await helper");
        let function: Function = lua
            .load("return function() return await('rs-agent', '{}') end")
            .eval()
            .expect("entry function");
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_by_host = Arc::clone(&dropped);
        let host = Arc::new(move |_: HostRequest, _: CancellationToken| {
            PendingUntilDropped(Arc::clone(&dropped_by_host))
        });
        let runtime = LuauAsyncRuntime::new(&lua, host);
        let mut task = runtime.start(function, ()).expect("start task");
        assert!(matches!(poll_once(&mut task), Poll::Pending));
        task.cancel();
        assert!(matches!(
            poll_once(&mut task),
            Poll::Ready(Err(AsyncRuntimeError::Cancelled))
        ));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn malformed_yield_is_a_typed_protocol_error() {
        let lua = lua();
        let function: Function = lua
            .load("return function() return coroutine.yield('not-an-await') end")
            .eval()
            .expect("entry function");
        let host = Arc::new(|_: HostRequest, _: CancellationToken| {
            std::future::ready(Ok::<_, HostCallError>("unused".to_owned()))
        });
        let runtime = LuauAsyncRuntime::new(&lua, host);
        let mut task = runtime.start(function, ()).expect("start task");
        assert!(matches!(
            poll_once(&mut task),
            Poll::Ready(Err(AsyncRuntimeError::Protocol { .. }))
        ));
    }
}
