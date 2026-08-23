//! Stable identifiers and provider-neutral scalar state.

use std::fmt;

/// A stable identifier for an agent conversation message.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct MessageId(pub u64);

/// A stable identifier for one agent execution.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct RunId(pub u64);

/// A stable identifier for one model turn.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct TurnId(pub u64);

/// A provider-supplied identifier for one assistant-requested tool invocation.
///
/// Unlike runtime-generated run and message counters, this remains textual so
/// Pi/provider call identifiers survive model context and result correlation
/// unchanged. An empty ID is rejected at the model boundary.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Construct a non-empty provider tool-call identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, ToolCallIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ToolCallIdError);
        }
        Ok(Self(value))
    }

    /// Borrow the provider's exact tool-call identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Error returned when a provider omits the required tool-call identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolCallIdError;

impl fmt::Display for ToolCallIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("tool-call ID cannot be empty")
    }
}

impl std::error::Error for ToolCallIdError {}

/// The model's reasoning budget selection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThinkingLevel {
    /// Disable additional reasoning where the provider supports it.
    #[default]
    Off,
    /// Request the smallest explicit reasoning budget.
    Minimal,
    /// Request a low reasoning budget.
    Low,
    /// Request a medium reasoning budget.
    Medium,
    /// Request a high reasoning budget.
    High,
    /// Request an extra-high reasoning budget.
    XHigh,
    /// Request the maximum reasoning budget.
    Max,
}

/// Provider-independent model identity.
///
/// There is intentionally no `Default` implementation: a host must choose a
/// concrete provider and model before binding a provider-backed run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelDescriptor {
    /// Provider name chosen by the host.
    pub provider: String,
    /// Provider model name.
    pub model: String,
    /// Optional revision or snapshot pin.
    pub revision: Option<String>,
}

/// A serialized JSON value at an integration boundary.
///
/// The core preserves this exact text in state and validates it only at the tool invocation
/// boundary. Provider and transport adapters may use the stable `tea_protocol::JsonValue`
/// representation without changing the state-machine contract.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SerializedJson(pub String);

impl SerializedJson {
    /// Construct a serialized JSON boundary value without implying validation.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the serialized representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
