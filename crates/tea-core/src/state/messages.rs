//! Canonical conversation messages and assistant tool calls.

use super::*;
use std::fmt;

/// Maximum retained size of one provider-private continuation item.
///
/// The core never interprets this material. One MiB accommodates a complete
/// high-reasoning continuation under the harness's unlimited-output contract;
/// the finite limit still prevents a malformed provider response from turning
/// opaque state into an unbounded durable side channel.
pub const MAX_OPAQUE_PROVIDER_CONTEXT_BYTES: usize = 1_048_576;
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

    #[test]
    fn opaque_context_allows_a_large_reasoning_record_but_keeps_a_finite_boundary() {
        assert!(
            OpaqueProviderContextItem::new(
                "openrouter",
                "reasoning_details",
                None,
                "r".repeat(90_000),
            )
            .is_ok()
        );
        assert!(matches!(
            OpaqueProviderContextItem::new(
                "openrouter",
                "reasoning_details",
                None,
                "r".repeat(MAX_OPAQUE_PROVIDER_CONTEXT_BYTES + 1),
            ),
            Err(OpaqueProviderContextError::PayloadTooLarge { .. })
        ));
    }
}

/// One ordered piece of assistant output.
///
/// Answer text and provider-exposed thinking are distinct: thinking is
/// reasoning the provider chose to show, never part of the answer. Each
/// thinking block may carry provider-private replay material (a signature or
/// encrypted continuation) that only the matching adapter may interpret; it is
/// never rendered or passed to tools.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssistantContent {
    /// Visible answer text.
    Text {
        /// Exact text.
        text: String,
    },
    /// Provider-exposed reasoning summary or text.
    Thinking {
        /// Visible thinking text. It may be empty when the provider withheld
        /// it but still issued replay material.
        text: String,
        /// Provider-private replay material for this block, when issued.
        signature: Option<OpaqueProviderContextItem>,
    },
    /// Reasoning withheld by the provider and represented only by opaque
    /// replay material.
    RedactedThinking {
        /// Provider-private encrypted block.
        data: OpaqueProviderContextItem,
    },
}

impl AssistantContent {
    /// Construct a text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Construct a thinking block without replay material.
    pub fn thinking(text: impl Into<String>) -> Self {
        Self::Thinking {
            text: text.into(),
            signature: None,
        }
    }
}

/// Concatenate the answer text of assistant content, excluding thinking.
pub fn assistant_text(content: &[AssistantContent]) -> String {
    let mut text = String::new();
    for block in content {
        if let AssistantContent::Text { text: block } = block {
            text.push_str(block);
        }
    }
    text
}

/// Concatenate the visible thinking text of assistant content.
pub fn assistant_thinking(content: &[AssistantContent]) -> String {
    let mut text = String::new();
    for block in content {
        if let AssistantContent::Thinking { text: block, .. } = block {
            if !text.is_empty() && !block.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(block);
        }
    }
    text
}

/// Approximate model-visible bytes of assistant content: answer and thinking
/// text, excluding provider-private replay material.
pub fn content_bytes(content: &[AssistantContent]) -> usize {
    content
        .iter()
        .map(|block| match block {
            AssistantContent::Text { text } | AssistantContent::Thinking { text, .. } => text.len(),
            AssistantContent::RedactedThinking { .. } => 0,
        })
        .sum()
}

/// Build the canonical content of an assistant reply containing only text.
pub fn text_content(text: impl Into<String>) -> Vec<AssistantContent> {
    let text = text.into();
    if text.is_empty() {
        Vec::new()
    } else {
        vec![AssistantContent::Text { text }]
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
        /// Ordered answer text and provider-exposed thinking.
        content: Vec<AssistantContent>,
        /// Source-ordered tool calls, after all content blocks.
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
        /// Physical model that produced this response, when known.
        ///
        /// Replay of thinking signatures and other provider-private material
        /// is valid only for the same physical model.
        origin: Option<ModelDescriptor>,
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
    /// Model-visible configuration at this point in conversation order.
    ///
    /// The first system message is the initial configuration; later ones
    /// change it. See [`EffectiveConfiguration::replay`].
    System {
        id: MessageId,
        update: ConfigurationUpdate,
    },
}

impl AgentMessage {
    /// Stable message identity.
    pub fn id(&self) -> MessageId {
        match self {
            Self::User { id, .. }
            | Self::Assistant { id, .. }
            | Self::ToolResult { id, .. }
            | Self::System { id, .. } => *id,
        }
    }

    /// Answer text of an assistant message, or `None` for other messages.
    pub fn assistant_text(&self) -> Option<String> {
        match self {
            Self::Assistant { content, .. } => Some(assistant_text(content)),
            _ => None,
        }
    }
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
