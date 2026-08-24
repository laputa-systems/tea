//! Public tool-handler metadata and construction validation.

use std::fmt;
use tea_core::tool::{CancellationSettlementMode, ToolExecutionMode};
use tea_protocol::JsonValue;

/// Resource limits applied independently to each handler invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HandlerLimits {
    /// Largest accepted handler source in bytes.
    pub max_source_bytes: usize,
    /// Largest Luau VM allocation total in bytes.
    pub max_memory_bytes: usize,
    /// Largest number of Luau interrupt checks permitted per coroutine resume.
    pub max_interrupt_checks: usize,
    /// Largest number of host capability operations permitted per invocation.
    ///
    /// This bounds I/O amplification even when a handler stays within its VM
    /// memory and instruction budgets.
    pub max_capability_calls: usize,
}

impl Default for HandlerLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
            max_capability_calls: 64,
        }
    }
}

/// Prompt-facing metadata for one Luau-backed tool.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolHandlerSpec {
    /// Stable tool name sent to the model.
    pub name: String,
    /// Prompt-facing explanation of the tool.
    pub description: String,
    /// JSON Schema for tool arguments.
    pub schema: JsonValue,
    /// Capability name the handler is permitted to request.
    pub capability: String,
    /// Whether the core may overlap calls to this tool.
    pub execution_mode: ToolExecutionMode,
    /// Whether this tool must be the sole call in an assistant batch.
    pub requires_exclusive_batch: bool,
    /// How a started invocation settles after run cancellation.
    pub cancellation_settlement_mode: CancellationSettlementMode,
}

/// Failure while loading or validating a Luau tool handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolHandlerInitError {
    /// Source exceeded the configured pre-VM boundary.
    SourceTooLarge {
        /// Received source length in bytes.
        actual: usize,
        /// Configured source limit in bytes.
        limit: usize,
    },
    /// A resource limit was zero.
    InvalidLimit {
        /// Stable configuration field name.
        field: &'static str,
    },
    /// Tool metadata or the handler return/yield contract was invalid.
    Contract {
        /// Contract validation diagnostic.
        message: String,
    },
    /// The declared capability was not supplied by the host.
    UnboundCapability {
        /// Capability declared by the handler but not supplied by the host.
        capability: String,
    },
    /// The sandboxed VM rejected the handler source.
    Runtime {
        /// Host-safe VM loading diagnostic.
        message: String,
    },
}

impl fmt::Display for ToolHandlerInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "handler source is {actual} bytes, exceeding {limit} bytes"
                )
            }
            Self::InvalidLimit { field } => write!(formatter, "handler limit {field} is zero"),
            Self::Contract { message } => write!(formatter, "invalid handler contract: {message}"),
            Self::UnboundCapability { capability } => write!(
                formatter,
                "handler capability {capability:?} was not explicitly bound"
            ),
            Self::Runtime { message } => {
                write!(formatter, "Luau handler failed to load: {message}")
            }
        }
    }
}

impl std::error::Error for ToolHandlerInitError {}

pub(super) fn validate_limits(limits: HandlerLimits) -> Result<(), ToolHandlerInitError> {
    for (field, value) in [
        ("max_source_bytes", limits.max_source_bytes),
        ("max_memory_bytes", limits.max_memory_bytes),
        ("max_interrupt_checks", limits.max_interrupt_checks),
        ("max_capability_calls", limits.max_capability_calls),
    ] {
        if value == 0 {
            return Err(ToolHandlerInitError::InvalidLimit { field });
        }
    }
    Ok(())
}

pub(super) fn validate_spec(spec: &ToolHandlerSpec) -> Result<(), ToolHandlerInitError> {
    for (field, value) in [
        ("name", spec.name.as_str()),
        ("description", spec.description.as_str()),
        ("capability", spec.capability.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(ToolHandlerInitError::Contract {
                message: format!("tool field {field:?} must not be empty"),
            });
        }
    }
    Ok(())
}
