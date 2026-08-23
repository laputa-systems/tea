//! Canonical conversation messages and assistant tool calls.

use super::*;

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
