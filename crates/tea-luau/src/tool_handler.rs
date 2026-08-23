//! Coroutine-backed Luau tool handlers.
//!
//! This module is the narrow bridge between a Luau extension and the core's
//! [`tea_core::tool::AgentTool`] contract. A handler is evaluated in a
//! fresh, sandboxed VM for each invocation. It can suspend only by yielding an
//! explicitly named capability request; the host resumes the coroutine after
//! the caller-owned capability future settles.
//!
//! The handler never creates an executor, spawns a task, resolves a capability
//! by ambient name, or performs I/O. A host supplies every capability through
//! [`CapabilityBindings`]. The implementation is split into metadata and
//! validation, explicit bindings, and runtime adaptation modules.

mod bindings;
mod runtime;
mod specs;

pub use bindings::{
    BindingError, CapabilityBindings, CapabilityError, CapabilityFuture, CapabilityRequest,
    CapabilityResponse, LuauCapability, PureCapability, PURE_CAPABILITY_V1,
};
pub use runtime::LuaToolHandler;
pub use specs::{HandlerLimits, ToolHandlerInitError, ToolHandlerSpec};
