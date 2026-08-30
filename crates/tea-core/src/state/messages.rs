//! Canonical conversation messages and assistant tool calls.

use super::*;
use std::fmt;

/// Maximum retained size of one provider-private continuation item.
///
/// The core never interprets this material.  A finite limit keeps a malformed
/// provider response from turning opaque continuation state into an unbounded
/// durable side channel.
pub const MAX_OPAQUE_PROVIDER_CONTEXT_BYTES: usize = 65_536;
/// Maximum UTF-8 byte length of a provider identity on an opaque item.
pub const MAX_OPAQUE_PROVIDER_CONTEXT_PROVIDER_BYTES: usize = 64;
/// Maximum UTF-8 byte length of a provider-defined opaque item kind.
pub const MAX_OPAQUE_PROVIDER_CONTEXT_KIND_BYTES: usize = 64;
/// Maximum UTF-8 byte length of a provider item identity.
pub const MAX_OPAQUE_PROVIDER_CONTEXT_ITEM_ID_BYTES: usize = 512;

/// A provider-scoped continuation item retained beside one assistant message.
///
/// This is deliberately separate from visible assistant content.  Adapters
/// may use it for opaque server-issued state such as encrypted reasoning
/// continuity, but ordinary transcript rendering and tools never receive it.
#[derive(Clone, Eq, PartialEq)]
pub struct OpaqueProviderContextItem {
    provider: String,
    kind: String,
    item_id: Option<String>,
    payload: String,
}

impl OpaqueProviderContextItem {
    /// Construct one bounded provider-private continuation item.
    pub fn new(
        provider: impl Into<String>,
        kind: impl Into<String>,
        item_id: Option<String>,
        payload: impl Into<String>,
    ) -> Result<Self, OpaqueProviderContextError> {
        let provider = provider.into();
        let kind = kind.into();
        let payload = payload.into();
        if provider.trim().is_empty() {
            return Err(OpaqueProviderContextError::EmptyProvider);
        }
        if provider.len() > MAX_OPAQUE_PROVIDER_CONTEXT_PROVIDER_BYTES {
            return Err(OpaqueProviderContextError::ProviderTooLong {
                maximum: MAX_OPAQUE_PROVIDER_CONTEXT_PROVIDER_BYTES,
                actual: provider.len(),
            });
        }
        if provider.chars().any(char::is_control) {
            return Err(OpaqueProviderContextError::UnsafeProvider);
        }
        if kind.trim().is_empty() {
            return Err(OpaqueProviderContextError::EmptyKind);
        }
        if kind.len() > MAX_OPAQUE_PROVIDER_CONTEXT_KIND_BYTES {
            return Err(OpaqueProviderContextError::KindTooLong {
                maximum: MAX_OPAQUE_PROVIDER_CONTEXT_KIND_BYTES,
                actual: kind.len(),
            });
        }
        if kind.chars().any(char::is_control) {
            return Err(OpaqueProviderContextError::UnsafeKind);
        }
        if let Some(item_id) = item_id.as_deref() {
            if item_id.is_empty() {
                return Err(OpaqueProviderContextError::EmptyItemId);
            }
            if item_id.len() > MAX_OPAQUE_PROVIDER_CONTEXT_ITEM_ID_BYTES {
                return Err(OpaqueProviderContextError::ItemIdTooLong {
                    maximum: MAX_OPAQUE_PROVIDER_CONTEXT_ITEM_ID_BYTES,
                    actual: item_id.len(),
                });
            }
            if item_id.chars().any(char::is_control) {
                return Err(OpaqueProviderContextError::UnsafeItemId);
            }
        }
        if payload.is_empty() {
            return Err(OpaqueProviderContextError::EmptyPayload);
        }
        if payload.len() > MAX_OPAQUE_PROVIDER_CONTEXT_BYTES {
            return Err(OpaqueProviderContextError::PayloadTooLarge {
                maximum: MAX_OPAQUE_PROVIDER_CONTEXT_BYTES,
                actual: payload.len(),
            });
        }
        Ok(Self {
            provider,
            kind,
            item_id,
            payload,
        })
    }

    /// Provider identifier allowed to interpret this item.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Provider-defined opaque item kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Provider item identity when the remote protocol supplies one.
    pub fn item_id(&self) -> Option<&str> {
        self.item_id.as_deref()
    }

    /// Exact opaque payload for the matching provider adapter.
    pub fn payload(&self) -> &str {
        &self.payload
    }
}

impl fmt::Debug for OpaqueProviderContextItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpaqueProviderContextItem")
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("item_id", &self.item_id)
            .field("payload", &"[redacted]")
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

/// Invalid provider-private continuation material at the core boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpaqueProviderContextError {
    /// The adapter omitted its provider identity.
    EmptyProvider,
    /// The provider identity contained a control character.
    UnsafeProvider,
    /// The provider identity exceeded its durable bound.
    ProviderTooLong {
        /// Maximum accepted UTF-8 byte length.
        maximum: usize,
        /// Actual UTF-8 byte length.
        actual: usize,
    },
    /// The adapter omitted its provider-defined item kind.
    EmptyKind,
    /// The provider-defined item kind contained a control character.
    UnsafeKind,
    /// The provider-defined item kind exceeded its durable bound.
    KindTooLong {
        /// Maximum accepted UTF-8 byte length.
        maximum: usize,
        /// Actual UTF-8 byte length.
        actual: usize,
    },
    /// The adapter supplied an empty item identity.
    EmptyItemId,
    /// The provider item identity contained a control character.
    UnsafeItemId,
    /// The provider item identity exceeded its durable bound.
    ItemIdTooLong {
        /// Maximum accepted UTF-8 byte length.
        maximum: usize,
        /// Actual UTF-8 byte length.
        actual: usize,
    },
    /// The adapter supplied an empty opaque payload.
    EmptyPayload,
    /// The payload exceeded the durable continuation limit.
    PayloadTooLarge {
        /// Maximum accepted UTF-8 byte length.
        maximum: usize,
        /// Actual UTF-8 byte length.
        actual: usize,
    },
}

impl fmt::Display for OpaqueProviderContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProvider => {
                formatter.write_str("opaque provider context requires a provider")
            }
            Self::UnsafeProvider => {
                formatter.write_str("opaque provider context provider is unsafe")
            }
            Self::ProviderTooLong { maximum, actual } => write!(
                formatter,
                "opaque provider context provider is too large: {actual} bytes exceeds {maximum} bytes"
            ),
            Self::EmptyKind => formatter.write_str("opaque provider context requires a kind"),
            Self::UnsafeKind => formatter.write_str("opaque provider context kind is unsafe"),
            Self::KindTooLong { maximum, actual } => write!(
                formatter,
                "opaque provider context kind is too large: {actual} bytes exceeds {maximum} bytes"
            ),
            Self::EmptyItemId => {
                formatter.write_str("opaque provider context item ID must not be empty")
            }
            Self::UnsafeItemId => formatter.write_str("opaque provider context item ID is unsafe"),
            Self::ItemIdTooLong { maximum, actual } => write!(
                formatter,
                "opaque provider context item ID is too large: {actual} bytes exceeds {maximum} bytes"
            ),
            Self::EmptyPayload => {
                formatter.write_str("opaque provider context payload must not be empty")
            }
            Self::PayloadTooLarge { maximum, actual } => write!(
                formatter,
                "opaque provider context payload is too large: {actual} bytes exceeds {maximum} bytes"
            ),
        }
    }
}

impl std::error::Error for OpaqueProviderContextError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_context_labels_are_bounded_before_they_reach_durable_state() {
        let item =
            || OpaqueProviderContextItem::new("codex", "reasoning", Some("rs_1".into()), "cipher");
        assert!(item().is_ok());
        assert!(matches!(
            OpaqueProviderContextItem::new(
                "p".repeat(MAX_OPAQUE_PROVIDER_CONTEXT_PROVIDER_BYTES + 1),
                "reasoning",
                None,
                "cipher",
            ),
            Err(OpaqueProviderContextError::ProviderTooLong { .. })
        ));
        assert!(matches!(
            OpaqueProviderContextItem::new(
                "codex",
                "reasoning",
                Some("i".repeat(MAX_OPAQUE_PROVIDER_CONTEXT_ITEM_ID_BYTES + 1)),
                "cipher",
            ),
            Err(OpaqueProviderContextError::ItemIdTooLong { .. })
        ));
        assert!(matches!(
            OpaqueProviderContextItem::new("codex", "reasoning\n", None, "cipher"),
            Err(OpaqueProviderContextError::UnsafeKind)
        ));
    }
}

/// A message retained in the canonical conversation history.
///
/// This is the Rust spelling of upstream Pi's `AgentMessage`. The core currently
/// has no application-defined message extension point, so the standard message
/// union is the complete agent-message contract.
#[allow(missing_docs)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentMessage {
    /// Host-provided user input.
    User { id: MessageId, content: String },
    /// Provider response, including any textual partial/final content.
    Assistant {
        id: MessageId,
        content: String,
        tool_calls: Vec<AgentToolCall>,
        /// Terminal model stop reason, when this is the finalized assistant message.
        /// `None` is used for a partial streaming snapshot.
        stop_reason: Option<StopReason>,
        /// Provider/model diagnostic for an error or aborted response.
        error_message: Option<String>,
        /// Provider-private opaque continuation state associated with this turn.
        ///
        /// It is durable and ordered with this assistant output, but never
        /// rendered as transcript text or exposed to tools.
        opaque_context: Vec<OpaqueProviderContextItem>,
    },
    /// Result injected after a tool invocation.
    ToolResult {
        id: MessageId,
        tool_call_id: ToolCallId,
        tool_name: String,
        content: String,
        details: Option<SerializedJson>,
        usage: Box<Option<Usage>>,
        added_tool_names: Vec<String>,
        /// Whether this finalized result requested the run stop after its batch.
        terminate: bool,
        is_error: bool,
        /// Typed host classification for an error result, when supplied.
        failure: Option<crate::tool::ToolFailure>,
    },
}

/// A tool call embedded in an assistant message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentToolCall {
    /// Stable call identifier.
    pub id: ToolCallId,
    /// Registered tool name.
    pub name: String,
    /// Serialized JSON arguments.
    pub arguments: SerializedJson,
}
