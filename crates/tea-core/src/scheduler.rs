//! Caller-owned scheduling seams and deterministic tool ordering.
//!
//! The scheduler plans work but does not create threads, an executor, or a background task.
//! For parallel batches, calls are prepared in assistant/source order, completions are emitted
//! in actual completion order, and context results are recovered in source order.

use crate::error::SchedulerError;
use crate::state::{
    AgentToolCall, ModelDescriptor, OpaqueProviderContextItem, ThinkingLevel, ToolCallId,
};
use crate::transcript::{ConfigurationProjection, Transcript};
use crate::tool::{AgentToolResult, ToolCall, ToolExecutionMode};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tea_session::ProviderErrorRecord;

/// A boxed provider stream operation, driven by the embedding executor.
pub type ModelFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn ModelEventStream>, SchedulerError>> + Send + 'a>>;

/// One asynchronously delivered provider event.
pub type ModelEventFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<ModelStreamEvent>, SchedulerError>> + Send + 'a>>;

/// Caller-polled assistant response event source.
///
/// A provider must resolve [`ModelProvider::stream`] as soon as it has a response source, then
/// yield each available event through this trait. The core never buffers a provider response
/// before reducing its deltas, and the provider receives the run's cancellation scope for every
/// poll. Implementations may bridge an HTTP body, a native model, a world RPC, or the finite
/// [`ModelStream`] test adapter without imposing a runtime on the core.
pub trait ModelEventStream: Send {
    /// Wait for the next event, or return `Ok(None)` when the source closes.
    ///
    /// A well-formed assistant response still needs a terminal [`ModelStreamEvent::End`],
    /// [`ModelStreamEvent::Error`], or [`ModelStreamEvent::Aborted`] before closing.
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a>;
}

/// A model response stream abstraction.  The provider owns transport and retry policy.
pub trait ModelProvider: Send + Sync {
    /// Start one inference request and return its incrementally polled event source.
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a>;

    /// Explicit capabilities of one physical model served by this provider.
    ///
    /// The default is deliberately conservative: no in-place configuration
    /// updates, no exposed thinking, no known prompt-cache lifetime, and no
    /// pricing. The core never infers a capability from a model name.
    fn capabilities(&self, _model: Option<&ModelDescriptor>) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Why a provider request is made.
///
/// Durable hosts and adapters distinguish request roles from this typed value,
/// never from prompt text.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RequestPurpose {
    /// An ordinary model turn of an agent run.
    #[default]
    Turn,
    /// A summary request owned by a compaction transaction.
    Compaction,
    /// A best-effort replay that keeps a provider prompt cache entry warm.
    /// Its output never enters model context and its tool calls never run.
    CacheMaintenance,
}

impl RequestPurpose {
    /// Stable lowercase label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Compaction => "compaction",
            Self::CacheMaintenance => "cache_maintenance",
        }
    }
}

/// Provider request assembled by the core.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelRequest {
    /// Role of this request.
    pub purpose: RequestPurpose,
    /// Ordered typed transcript, including system configuration messages.
    ///
    /// The current system prompt and declared tools are derived from it; see
    /// [`Self::system_prompt`] and [`Self::tools`].
    pub transcript: Transcript,
    /// Physical model identity this request is dispatched to.
    pub model: Option<ModelDescriptor>,
    /// Model selected by the user or host when it differs from the physical
    /// dispatch (for example a virtual model resolved by a router).
    pub selected_model: Option<ModelDescriptor>,
    /// Reasoning level selected for this request.
    ///
    /// This is request-scoped: a `prepare_next_turn` hook may replace it for a
    /// later turn without mutating the agent's configured default.
    pub thinking_level: ThinkingLevel,
    /// Stable durable-session identity when the host owns one.
    ///
    /// This is opaque provider metadata, never user content. Providers that
    /// support session/cache correlation may use it for a stable request
    /// header or cache key; a missing value means the embedding has no durable
    /// session identity for this request.
    pub session_id: Option<String>,
    /// Request-specific output-token cap, overriding the adapter default.
    ///
    /// Cache maintenance uses this to request minimal output. Adapters for
    /// which a smaller cap changes cache-relevant request content must not
    /// declare cache-maintenance replay as safe.
    pub max_output_tokens: Option<u32>,
}

impl ModelRequest {
    /// The system prompt in force at the end of the transcript.
    pub fn system_prompt(&self) -> String {
        self.transcript.system_prompt()
    }

    /// The tool declarations in force at the end of the transcript.
    pub fn tools(&self) -> Vec<crate::tool::ToolDeclaration> {
        self.transcript.tools()
    }
}

/// Explicit, provider-declared facts about one physical model.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelCapabilities {
    /// How the transport carries configuration changes after the first message.
    pub configuration_updates: ConfigurationUpdateSupport,
    /// Whether the provider can return thinking separately from answer text.
    pub exposes_thinking: bool,
    /// Prompt-cache lifetimes and replay safety, when declared.
    pub prompt_cache: Option<PromptCacheCapability>,
    /// Exact token prices, when declared.
    pub pricing: Option<ModelPricing>,
    /// Known context capacity in tokens.
    pub context_window: Option<u64>,
}

impl ModelCapabilities {
    /// The configuration projection used by this transport.
    pub fn configuration_projection(&self) -> ConfigurationProjection {
        if self.configuration_updates.system_messages {
            ConfigurationProjection::InPlace
        } else {
            ConfigurationProjection::Collapsed
        }
    }
}

/// In-place configuration-update support.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConfigurationUpdateSupport {
    /// Later system messages are accepted in place.
    pub system_messages: bool,
    /// Tool additions and removals are accepted in place.
    pub tool_changes: bool,
}

/// Declared prompt-cache behavior of one model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptCacheCapability {
    /// Lifetime in seconds of the cache entry an ordinary request writes.
    pub ttl_seconds: u64,
    /// Whether replaying an admitted request with a one-token output cap
    /// reuses its cache entry.
    pub minimal_output_replay: MinimalOutputReplay,
}

/// Whether a one-token replay of an admitted request is cache-equivalent.
///
/// Reducing the output limit is not cache-neutral for every model: for
/// example, budget-based thinking derives its budget from the output cap,
/// which changes cache-relevant request content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MinimalOutputReplay {
    /// Cache-equivalent for every request to this model.
    Safe,
    /// Cache-equivalent only when the request has thinking off.
    SafeWithoutThinking,
    /// Never cache-equivalent.
    Unsafe,
}

/// Exact token prices in decimal dollars per million tokens.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelPricing {
    /// Uncached input tokens.
    pub input: String,
    /// Output tokens.
    pub output: String,
    /// Cache-read input tokens.
    pub cache_read: String,
    /// Cache-write input tokens.
    pub cache_write: String,
}

/// Content-safe request facts observed by a provider adapter.
///
/// The core deliberately does not know a provider's wire schema or cache-key
/// rules. An adapter may nevertheless report the exact byte count it sent and
/// fingerprints for cache-relevant envelope components. Component names are
/// stable, provider-neutral labels selected by the adapter; values are
/// diagnostic fingerprints, never raw headers, prompts, credentials, or
/// provider-specific structs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdapterRequestObservation {
    /// Core-measured shared prefix with the preceding logical request in this
    /// run. `None` means no predecessor exists; zero means no bytes matched.
    /// This is a cacheability proxy, never a provider cache-hit claim.
    pub deterministic_common_prefix_bytes: Option<u64>,
    /// Stable rough token estimate for the deterministic shared prefix.
    pub deterministic_common_prefix_tokens_estimate: Option<u64>,
    /// Byte count of the exact serialized request supplied to transport.
    pub serialized_request_bytes: Option<usize>,
    /// Adapter-defined cache-domain fingerprint, when the adapter can expose one safely.
    pub cache_domain_fingerprint: Option<u64>,
    /// Fingerprints for individual cache-relevant envelope components.
    pub cache_domain_components: BTreeMap<String, u64>,
    /// Provider request identifier, if the provider exposes one without requiring a follow-up.
    pub provider_request_id: Option<String>,
}

/// Provider events consumed by the run loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelStreamEvent {
    /// Content-safe facts about the exact request sent by the adapter.
    ///
    /// Adapters emit this at most once and before a terminal event. It is an
    /// observation, not a cache-hit claim and not a second preparation path.
    RequestObservation(AdapterRequestObservation),
    /// Incremental assistant text.
    TextDelta(String),
    /// Incremental provider-exposed thinking text.
    ///
    /// Consecutive thinking deltas form one thinking block until a signature,
    /// another content kind, or a tool call ends it.
    ThinkingDelta(String),
    /// Provider-private replay material that ends the current thinking block.
    ///
    /// When no thinking block is open, it ends an empty one. The core stores
    /// it with that block and never renders it.
    ThinkingSignature(OpaqueProviderContextItem),
    /// A complete reasoning block withheld by the provider.
    RedactedThinking(OpaqueProviderContextItem),
    /// A complete assistant tool call.
    ToolCall(AgentToolCall),
    /// Provider-private continuation state associated with the current assistant output.
    ///
    /// The core persists this beside the settled assistant message but never
    /// renders it or passes it to tools. Only an adapter with the matching
    /// provider ID may replay it on a later request.
    OpaqueProviderContext(OpaqueProviderContextItem),
    /// Provider usage update.
    Usage(crate::state::Usage),
    /// Provider/model failure represented as a terminal assistant response.
    ///
    /// This is distinct from a rejected [`ModelProvider::stream`] future: the
    /// provider successfully returned a response stream, and its final
    /// assistant message carries this diagnostic with `StopReason::Error`.
    Error {
        /// Redacted provider/model diagnostic.
        message: String,
    },
    /// Typed bounded provider failure evidence accompanying [`Self::Error`].
    ///
    /// Adapters emit this before the terminal error event; it is not itself a
    /// terminal event and is ignored by compaction-only consumers.
    ProviderError(ProviderErrorRecord),
    /// The provider explicitly identified the incomplete response as a
    /// context-capacity overflow. This is typed so the generic core never
    /// guesses from an HTTP body or error string.
    ContextOverflow {
        /// Redacted provider diagnostic.
        message: String,
    },
    /// Provider/model cancellation represented as a terminal assistant response.
    ///
    /// This is distinct from host cancellation: the provider independently
    /// stopped the response and supplied the final assistant diagnostic.
    Aborted {
        /// Redacted provider/model diagnostic.
        message: String,
    },
    /// Normal stream settlement.
    End(crate::state::StopReason),
}

/// A finite provider event stream for deterministic tests and recorded replays.
///
/// Production adapters normally return their own [`ModelEventStream`] implementation. This
/// concrete type deliberately remains available so fixture providers can construct an exact,
/// dependency-free sequence with a struct literal.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelStream {
    /// Events in provider order for a recorded or deterministic provider.
    pub events: Vec<ModelStreamEvent>,
}

impl ModelEventStream for ModelStream {
    fn next_event<'a>(&'a mut self, _cancellation: CancellationToken) -> ModelEventFuture<'a> {
        let event = if self.events.is_empty() {
            None
        } else {
            Some(self.events.remove(0))
        };
        Box::pin(std::future::ready(Ok(event)))
    }
}

/// Shared cancellation state with idempotent cancellation.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<CancellationState>);

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    next_waiter_id: std::sync::atomic::AtomicU64,
    waiters: Mutex<Vec<(u64, Waker)>>,
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancellationToken {
    /// Create a fresh uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation.  Repeated calls are harmless.
    pub fn cancel(&self) {
        if self.0.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let waiters = std::mem::take(
            &mut *self
                .0
                .waiters
                .lock()
                .expect("cancellation waiter mutex poisoned"),
        );
        for (_, waiter) in waiters {
            waiter.wake();
        }
    }

    /// Check whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Register a scheduler waker for cancellation without allocating a
    /// short-lived wait future. Internal polling loops use this to ensure an
    /// otherwise cancellation-unaware capability cannot leave a run busy.
    pub(crate) fn register_waker(&self, waker: &Waker) {
        if self.is_cancelled() {
            waker.wake_by_ref();
            return;
        }
        let mut waiters = self
            .0
            .waiters
            .lock()
            .expect("cancellation waiter mutex poisoned");
        if self.is_cancelled() {
            waker.wake_by_ref();
            return;
        }
        if !waiters
            .iter()
            .any(|(_, existing)| existing.will_wake(waker))
        {
            let id = self.0.next_waiter_id.fetch_add(1, Ordering::Relaxed);
            waiters.push((id, waker.clone()));
        }
    }

    /// Return a future that resolves as soon as this token is cancelled.
    ///
    /// Provider, tool, and hook adapters can race this future against their own I/O without
    /// polling an atomic or depending on any executor-specific cancellation primitive.
    pub fn cancelled(&self) -> CancellationWait {
        CancellationWait {
            token: self.clone(),
            waiter_id: self.0.next_waiter_id.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// Future returned by [`CancellationToken::cancelled`].
#[derive(Debug)]
pub struct CancellationWait {
    token: CancellationToken,
    waiter_id: u64,
}

impl Future for CancellationWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        let mut waiters = self
            .token
            .0
            .waiters
            .lock()
            .expect("cancellation waiter mutex poisoned");
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        if let Some((_, waiter)) = waiters
            .iter_mut()
            .find(|(waiter_id, _)| *waiter_id == self.waiter_id)
        {
            if !waiter.will_wake(context.waker()) {
                *waiter = context.waker().clone();
            }
        } else {
            waiters.push((self.waiter_id, context.waker().clone()));
        }
        Poll::Pending
    }
}

impl Drop for CancellationWait {
    fn drop(&mut self) {
        if self.token.is_cancelled() {
            return;
        }
        self.token
            .0
            .waiters
            .lock()
            .expect("cancellation waiter mutex poisoned")
            .retain(|(waiter_id, _)| *waiter_id != self.waiter_id);
    }
}

/// One source-ordered tool call and its scheduler policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedToolCall {
    /// Zero-based source index in the assistant message.
    pub source_index: usize,
    /// Call payload.
    pub call: ToolCall,
    /// Registered execution policy.
    pub execution_mode: ToolExecutionMode,
}

/// A planned assistant tool batch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolBatch {
    /// Calls in assistant/source order.
    pub calls: Vec<PlannedToolCall>,
}

impl ToolBatch {
    /// Prepare calls in source order.  Actual execution is deliberately separate.
    pub fn prepare(calls: impl IntoIterator<Item = (ToolCall, ToolExecutionMode)>) -> Self {
        Self {
            calls: calls
                .into_iter()
                .enumerate()
                .map(|(source_index, (call, execution_mode))| PlannedToolCall {
                    source_index,
                    call,
                    execution_mode,
                })
                .collect(),
        }
    }

    /// Record a completion into a source-order result set.
    pub fn record_completion(
        &self,
        results: &mut CompletionSet,
        result: AgentToolResult,
    ) -> Result<(), SchedulerError> {
        if !self
            .calls
            .iter()
            .any(|call| call.call.id == result.tool_call_id)
        {
            return Err(SchedulerError::UnknownToolCall {
                tool_call_id: result.tool_call_id,
            });
        }
        results.insert(result)
    }
}

/// Tool completions keyed by call ID and emitted in source order when settled.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompletionSet {
    results: BTreeMap<ToolCallId, AgentToolResult>,
}

impl CompletionSet {
    fn insert(&mut self, result: AgentToolResult) -> Result<(), SchedulerError> {
        let tool_call_id = result.tool_call_id.clone();
        if self.results.insert(tool_call_id.clone(), result).is_some() {
            return Err(SchedulerError::DuplicateCompletion { tool_call_id });
        }
        Ok(())
    }

    /// Return settled results in assistant/source order, excluding incomplete calls.
    pub fn in_source_order(&self, batch: &ToolBatch) -> Vec<AgentToolResult> {
        batch
            .calls
            .iter()
            .filter_map(|call| self.results.get(&call.call.id).cloned())
            .collect()
    }

    /// Number of completed calls.
    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Whether no calls have completed.
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }
}

/// Stateless planning facade kept separate from agent ownership.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scheduler;

impl Scheduler {
    /// Build a source-ordered batch.  The caller decides which allowed calls to execute
    /// concurrently and reports their actual completion order through [`ToolBatch::record_completion`].
    pub fn plan_tool_batch(
        &self,
        calls: impl IntoIterator<Item = (ToolCall, ToolExecutionMode)>,
    ) -> ToolBatch {
        ToolBatch::prepare(calls)
    }
}
