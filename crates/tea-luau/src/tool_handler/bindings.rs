//! Explicit host capability bindings for Luau tool handlers.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tea_core::scheduler::CancellationToken;
use tea_core::state::ToolCallId;
use tea_core::tool::ToolUpdateSink;
use tea_protocol::JsonValue;

/// Stable closed-catalog name for a handler that performs only deterministic
/// computation inside its sandboxed Luau VM. A handler with this grant may
/// return a value directly, but any attempt to yield an external operation is
/// denied by [`PureCapability`].
pub const PURE_CAPABILITY_V1: &str = "tea.pure.v1";

/// A host capability request yielded by a Luau handler.
#[derive(Clone, Debug)]
pub struct CapabilityRequest {
    /// Provider call that owns this capability request.
    pub call_id: ToolCallId,
    /// Model-visible tool whose handler yielded this request.
    ///
    /// Hosts use this stable name when narrowing a shared capability to an
    /// exact model-facing operation, such as one MCP tool or resource method.
    pub tool_name: String,
    /// Core-owned durable attribution for this tool invocation.
    pub provenance: tea_core::effect::RunProvenance,
    /// Explicit capability binding selected by the host.
    pub capability: String,
    /// Method inside the capability, interpreted by the bound host object.
    pub method: String,
    /// Parsed JSON arguments supplied by the handler.
    pub arguments: JsonValue,
    /// Host update sink for progress or partial output.
    pub updates: ToolUpdateSink,
}

/// A successful capability response passed back into the suspended coroutine.
#[derive(Clone, Debug)]
pub struct CapabilityResponse {
    /// JSON value made available as the result of the yielded capability call.
    pub value: JsonValue,
}

/// A typed failure at the Luau capability boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    /// The operation was cancelled by the owning core run.
    Cancelled,
    /// The requested capability was not explicitly bound.
    NotBound {
        /// Requested capability name.
        capability: String,
    },
    /// The bound capability rejected the requested method.
    MethodDenied {
        /// Bound capability name.
        capability: String,
        /// Rejected method name.
        method: String,
    },
    /// Capability arguments failed host-side validation.
    InvalidArguments {
        /// Host validation diagnostic.
        message: String,
    },
    /// The bound host capability failed.
    Execution {
        /// Host failure diagnostic.
        message: String,
    },
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("capability operation was cancelled"),
            Self::NotBound { capability } => {
                write!(
                    formatter,
                    "capability {capability:?} is not explicitly bound"
                )
            }
            Self::MethodDenied { capability, method } => write!(
                formatter,
                "capability {capability:?} denied method {method:?}"
            ),
            Self::InvalidArguments { message } => {
                write!(formatter, "invalid capability arguments: {message}")
            }
            Self::Execution { message } => write!(formatter, "capability failed: {message}"),
        }
    }
}

impl std::error::Error for CapabilityError {}

/// A capability future owned by the caller's executor.
pub type CapabilityFuture =
    Pin<Box<dyn Future<Output = Result<CapabilityResponse, CapabilityError>> + Send + 'static>>;

/// An explicit host capability callable by a Luau handler.
pub trait LuauCapability: Send + Sync {
    /// Start one capability operation.
    ///
    /// The returned future must own any state it needs. The handler stores and
    /// polls it without selecting an executor or creating a task.
    fn invoke(
        &self,
        request: CapabilityRequest,
        cancellation: CancellationToken,
    ) -> CapabilityFuture;
}

/// The no-world-effect capability used for pure JSON/text transformations and
/// deterministic computation. It exists because every handler declares an
/// explicit capability even when it never yields; yielding through this grant
/// is always denied instead of becoming an ambient extension point.
#[derive(Clone, Copy, Debug, Default)]
pub struct PureCapability;

impl LuauCapability for PureCapability {
    fn invoke(
        &self,
        request: CapabilityRequest,
        _cancellation: CancellationToken,
    ) -> CapabilityFuture {
        Box::pin(std::future::ready(Err(CapabilityError::MethodDenied {
            capability: PURE_CAPABILITY_V1.into(),
            method: request.method,
        })))
    }
}

/// Failure while constructing an explicit capability binding set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingError {
    /// A binding name was empty.
    EmptyName,
    /// A capability name was already bound.
    Duplicate {
        /// Name that was already bound.
        capability: String,
    },
}

impl fmt::Display for BindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => formatter.write_str("capability binding name cannot be empty"),
            Self::Duplicate { capability } => {
                write!(formatter, "capability {capability:?} is already bound")
            }
        }
    }
}

impl std::error::Error for BindingError {}

/// An ordered-by-name set of capabilities explicitly granted by a host.
#[derive(Clone, Default)]
pub struct CapabilityBindings {
    entries: BTreeMap<String, Arc<dyn LuauCapability>>,
}

impl fmt::Debug for CapabilityBindings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityBindings")
            .field("names", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl CapabilityBindings {
    /// Create an empty binding set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one explicitly named capability.
    pub fn insert(
        &mut self,
        capability: impl Into<String>,
        implementation: Arc<dyn LuauCapability>,
    ) -> Result<(), BindingError> {
        let capability = capability.into();
        if capability.trim().is_empty() {
            return Err(BindingError::EmptyName);
        }
        if self.entries.contains_key(&capability) {
            return Err(BindingError::Duplicate { capability });
        }
        self.entries.insert(capability, implementation);
        Ok(())
    }

    /// Look up only a capability that was explicitly inserted by the host.
    pub fn get(&self, capability: &str) -> Option<Arc<dyn LuauCapability>> {
        self.entries.get(capability).cloned()
    }

    /// Return explicitly bound capability names in deterministic order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}
