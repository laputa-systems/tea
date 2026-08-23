//! Typed policy hooks around tool execution and context preparation.
//!
//! Hooks can influence a run only through explicit return values.  They never mutate agent
//! state directly, and hook failures remain typed so the run loop can apply the pinned
//! settlement policy.

use crate::error::HookError;
use crate::scheduler::CancellationToken;
use crate::state::{AgentMessage, ModelDescriptor, SerializedJson, ThinkingLevel, Usage};
use crate::tool::{AgentToolResult, ToolCall};
use std::future::Future;
use std::pin::Pin;

/// A caller-driven asynchronous hook operation.
///
/// The core only awaits this future on the embedding executor. Hooks receive
/// the run cancellation token and must settle after cancellation rather than
/// spawning detached work.
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, HookError>> + Send + 'a>>;

/// Decision made before a tool is executed.
#[allow(missing_docs)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BeforeToolCall {
    /// Proceed with execution.
    Allow,
    /// Replace the arguments that will be validated and sent to the
    /// capability.
    ///
    /// The replacement is not trusted merely because a hook returned it:
    /// `tea-core` runs the registered tool's canonical schema validator after
    /// this decision and before opening the external-effect boundary.
    Normalize {
        /// Complete JSON arguments for the same immutable tool-call ID/name.
        arguments: SerializedJson,
    },
    /// Convert the call into an error tool result.
    Block { reason: String },
    /// End the current run after recording the policy reason.
    Terminate { reason: String },
}

/// Optional replacement for one result field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Replacement<T> {
    /// Leave the value produced by the tool unchanged.
    #[default]
    Keep,
    /// Replace the value completely.
    Replace(T),
}

/// Changes an after-tool hook may make, with replacement rather than recursive merge semantics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AfterToolCall {
    /// Content replacement.
    pub content: Replacement<String>,
    /// Details replacement.
    pub details: Replacement<Option<SerializedJson>>,
    /// Error-flag replacement.
    pub is_error: Replacement<bool>,
    /// Typed host failure classification replacement.
    ///
    /// This lets a policy hook classify a raw capability error without the
    /// generic scheduler inspecting a diagnostic string.
    pub failure: Replacement<Option<crate::tool::ToolFailure>>,
    /// Usage replacement for providers that attach usage to tool results.
    pub usage: Replacement<Usage>,
    /// Dynamic capability names attached to the result for a subsequent host
    /// policy decision. The core records them but never silently registers a
    /// capability it was not explicitly given.
    pub added_tool_names: Replacement<Vec<String>>,
    /// Optional replacement for the batch early-termination hint.
    ///
    /// Only `Some(true)` participates in Pi's rule that every finalized call in a batch must
    /// request termination before the next model turn is suppressed.
    pub terminate: Option<bool>,
}

/// Versioned host-message envelope passed through context hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextEnvelope {
    /// Version of this host extension envelope.
    pub version: u16,
    /// Messages retained by the core.
    pub messages: Vec<AgentMessage>,
    /// Optional serialized host-only additions.
    pub host_messages: Vec<SerializedJson>,
}

/// Optional request-scoped replacements selected after a completed turn.
///
/// The upstream Pi loop applies these values after `turn_end` and before
/// `shouldStopAfterTurn`, steering polling, and the next provider request.
/// They belong to a run rather than durable [`AgentState`](crate::state::AgentState): a
/// subsequent `prompt` begins again from the builder-supplied model and
/// thinking defaults.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentLoopTurnUpdate {
    /// Replacement conversation envelope for the next request.
    pub context: Option<ContextEnvelope>,
    /// Replacement model identity for the next request.
    pub model: Option<ModelDescriptor>,
    /// Replacement reasoning level for the next request.
    pub thinking_level: Option<ThinkingLevel>,
}

/// Hook trait implemented by the embedding policy layer.
pub trait HookSet: Send + Sync {
    /// Decide whether one tool call may execute.
    fn before_tool_call(&self, call: &ToolCall) -> Result<BeforeToolCall, HookError>;
    /// Replace selected fields after execution.
    fn after_tool_call(
        &self,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<AfterToolCall, HookError>;
    /// Transform retained context before conversion to provider messages.
    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError>;
    /// Convert the host envelope into the provider's serialized context format.
    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError>;
    /// Decide whether the run should stop after the turn.
    fn should_stop_after_turn(&self, _context: &ContextEnvelope) -> Result<bool, HookError> {
        Ok(false)
    }
    /// Prepare request-scoped context, model, or reasoning replacements for the next turn.
    fn prepare_next_turn(
        &self,
        _context: ContextEnvelope,
    ) -> Result<AgentLoopTurnUpdate, HookError> {
        Ok(AgentLoopTurnUpdate::default())
    }

    /// Asynchronous, cancellation-aware form of [`Self::before_tool_call`].
    ///
    /// Existing synchronous policy remains useful for cheap decisions. An
    /// asynchronous policy can override this method to await an explicit
    /// capability without changing scheduler ownership.
    fn before_tool_call_async<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ContextEnvelope,
        _cancellation: CancellationToken,
    ) -> HookFuture<'a, BeforeToolCall> {
        Box::pin(std::future::ready(self.before_tool_call(call)))
    }

    /// Asynchronous, cancellation-aware form of [`Self::after_tool_call`].
    fn after_tool_call_async<'a>(
        &'a self,
        call: &'a ToolCall,
        result: &'a AgentToolResult,
        _context: ContextEnvelope,
        _cancellation: CancellationToken,
    ) -> HookFuture<'a, AfterToolCall> {
        Box::pin(std::future::ready(self.after_tool_call(call, result)))
    }

    /// Asynchronous, cancellation-aware form of [`Self::transform_context`].
    fn transform_context_async<'a>(
        &'a self,
        context: ContextEnvelope,
        _cancellation: CancellationToken,
    ) -> HookFuture<'a, ContextEnvelope> {
        Box::pin(std::future::ready(self.transform_context(context)))
    }

    /// Asynchronous, cancellation-aware form of [`Self::convert_to_llm`].
    fn convert_to_llm_async<'a>(
        &'a self,
        context: ContextEnvelope,
        _cancellation: CancellationToken,
    ) -> HookFuture<'a, String> {
        Box::pin(std::future::ready(self.convert_to_llm(context)))
    }

    /// Asynchronous, cancellation-aware form of [`Self::should_stop_after_turn`].
    fn should_stop_after_turn_async<'a>(
        &'a self,
        context: &'a ContextEnvelope,
        _cancellation: CancellationToken,
    ) -> HookFuture<'a, bool> {
        Box::pin(std::future::ready(self.should_stop_after_turn(context)))
    }

    /// Asynchronous, cancellation-aware form of [`Self::prepare_next_turn`].
    fn prepare_next_turn_async<'a>(
        &'a self,
        context: ContextEnvelope,
        _cancellation: CancellationToken,
    ) -> HookFuture<'a, AgentLoopTurnUpdate> {
        Box::pin(std::future::ready(self.prepare_next_turn(context)))
    }
}

/// A no-op hook implementation suitable as the default.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHooks;

impl HookSet for NoHooks {
    fn before_tool_call(&self, _call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        Ok(BeforeToolCall::Allow)
    }
    fn after_tool_call(
        &self,
        _call: &ToolCall,
        _result: &AgentToolResult,
    ) -> Result<AfterToolCall, HookError> {
        Ok(AfterToolCall::default())
    }
    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        Ok(context)
    }
    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        Ok(context
            .messages
            .into_iter()
            .map(|message| format!("{message:?}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}
