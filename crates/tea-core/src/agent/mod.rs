//! Agent ownership and configuration.
//!
//! An [`Agent`] owns durable conversation state and permits exactly one active [`RunHandle`].
//! It has no executor and no provider implementation; callers configure those explicit
//! capabilities and drive the run from their own async environment.

use crate::effect::{EffectGate, NoopEffectGate, RunProvenance};
use crate::error::CoreError;
use crate::event::EventObserver;
use crate::hooks::HookSet;
use crate::queue::{AgentQueues, QueueMode, QueuedMessage};
use crate::run::RunHandle;
use crate::scheduler::{CancellationToken, ModelProvider};
use crate::state::{AgentMessage, AgentPhase, AgentSnapshot, AgentState, ModelDescriptor};
use crate::tool::ToolRegistry;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Poll, Waker};

mod builder;
pub use builder::AgentBuilder;

/// Internal shared ownership record used by `Agent` and its run handle.
pub(crate) struct AgentInner {
    /// Durable and transient state.
    pub(crate) state: Mutex<AgentState>,
    /// Explicit queue state, not a general mailbox.
    pub(crate) queues: Mutex<AgentQueues>,
    /// Current run ownership marker.
    pub(crate) active_run: Mutex<Option<ActiveRun>>,
    /// Prompt, executable capabilities, and policy hooks selected for future runs.
    ///
    /// The lock is always acquired after the agent state lock. A run clones the
    /// `Arc` while holding both locks, then uses that immutable snapshot for its
    /// entire lifetime.
    pub(crate) configuration: RwLock<Arc<AgentConfiguration>>,
    /// Drain policy for messages that steer an active run.
    pub(crate) steering_mode: Mutex<QueueMode>,
    /// Drain policy for messages that run only at the idle boundary.
    pub(crate) follow_up_mode: Mutex<QueueMode>,
    /// Optional model provider, driven externally.
    pub(crate) provider: RwLock<Option<Arc<dyn ModelProvider>>>,
    /// Optional caller-supplied compactor, driven externally.
    pub(crate) compactor: RwLock<Option<Arc<dyn crate::compaction::Compactor>>>,
    /// Opt-in automatic-compaction policy; mutable counters stay on each run.
    pub(crate) automatic_compaction: RwLock<crate::compaction::AutomaticCompactionPolicy>,
    /// Bounded model-facing presentation for canonical tool results.
    pub(crate) tool_result_projection: crate::tool::ToolResultProjectionPolicy,
    /// Immutable circuit-breaker policy; streak state is allocated per run.
    pub(crate) tool_failure_circuit_breaker: crate::tool::ToolFailureCircuitBreaker,
    /// Awaited observers in registration order.
    pub(crate) observers: Mutex<Vec<ObserverRegistration>>,
    /// Monotonic process-local observer registrations.
    pub(crate) next_observer_id: AtomicU64,
    /// Non-blocking event subscribers that do not participate in settlement.
    pub(crate) subscribers: Mutex<Vec<SubscriberRegistration>>,
    /// Monotonic process-local non-blocking subscription registrations.
    pub(crate) next_subscriber_id: AtomicU64,
    /// Lossless live event subscribers that do not participate in settlement.
    pub(crate) lossless_subscribers: Mutex<Vec<LosslessSubscriberRegistration>>,
    /// Monotonic process-local lossless subscription registrations.
    pub(crate) next_lossless_subscriber_id: AtomicU64,
    /// Monotonic process-local run IDs.
    pub(crate) next_run_id: AtomicU64,
    /// Wakers awaiting the post-settlement idle boundary.
    pub(crate) idle_notifier: IdleNotifier,
}

/// A small executor-neutral idle notification primitive.
///
/// The agent owns no runtime, so this keeps only the wakers supplied by an
/// embedding executor. A settlement drains and wakes them after it has made
/// the agent idle.
#[derive(Default)]
pub(crate) struct IdleNotifier {
    waiters: Mutex<Vec<Waker>>,
}

impl IdleNotifier {
    fn register(&self, waker: &Waker) {
        let mut waiters = self.waiters.lock().expect("idle waiter mutex poisoned");
        if !waiters.iter().any(|existing| existing.will_wake(waker)) {
            waiters.push(waker.clone());
        }
    }

    pub(crate) fn notify(&self) {
        let waiters =
            std::mem::take(&mut *self.waiters.lock().expect("idle waiter mutex poisoned"));
        for waker in waiters {
            waker.wake();
        }
    }
}

/// Shared active-run marker, allowing `Agent::abort` to reach the handle's state without
/// keeping a second owning handle alive.
#[derive(Clone)]
pub(crate) struct ActiveRun {
    pub(crate) id: crate::state::RunId,
    pub(crate) state: Arc<Mutex<crate::state::RunState>>,
    pub(crate) cancellation: CancellationToken,
}

/// An owned agent state machine.
#[derive(Clone)]
pub struct Agent {
    pub(crate) inner: Arc<AgentInner>,
}

/// Prompt and host policy used by each newly started run.
///
/// An [`Agent`] owns one configuration for future runs. [`Agent::replace_configuration`]
/// swaps all three fields together while the agent is idle; an active run retains
/// the immutable snapshot it captured when ownership was reserved.
pub struct AgentConfiguration {
    /// System instructions sent with each model request.
    pub system_prompt: String,
    /// Ordered executable capabilities exposed to the model and tool scheduler.
    pub tools: ToolRegistry,
    /// Host policy hooks used at context and tool lifecycle boundaries.
    pub hooks: Arc<dyn HookSet>,
    /// Host-owned barrier around externally observable execution effects.
    ///
    /// The core invokes this immutable handle before and after provider, tool,
    /// and hook effects. It never gives the gate filesystem or session
    /// ownership; a durable supervisor supplies that implementation.
    pub effect_gate: Arc<dyn EffectGate>,
    /// Opaque durable attribution attached to every effect in a run.
    pub provenance: RunProvenance,
}

impl AgentConfiguration {
    /// Construct a prompt, tool registry, and hook set for future runs.
    pub fn new(
        system_prompt: impl Into<String>,
        tools: ToolRegistry,
        hooks: Arc<dyn HookSet>,
    ) -> Self {
        Self::with_effect_gate(
            system_prompt,
            tools,
            hooks,
            Arc::new(NoopEffectGate),
            RunProvenance::default(),
        )
    }

    /// Construct configuration with an explicit effect gate and provenance.
    pub fn with_effect_gate(
        system_prompt: impl Into<String>,
        tools: ToolRegistry,
        hooks: Arc<dyn HookSet>,
        effect_gate: Arc<dyn EffectGate>,
        provenance: RunProvenance,
    ) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            tools,
            hooks,
            effect_gate,
            provenance,
        }
    }
}

impl Clone for AgentConfiguration {
    fn clone(&self) -> Self {
        Self {
            system_prompt: self.system_prompt.clone(),
            tools: self.tools.clone(),
            hooks: Arc::clone(&self.hooks),
            effect_gate: Arc::clone(&self.effect_gate),
            provenance: self.provenance.clone(),
        }
    }
}

impl std::fmt::Debug for AgentConfiguration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfiguration")
            .field("system_prompt", &self.system_prompt)
            .field("tools", &self.tools)
            .field("provenance", &self.provenance)
            .finish_non_exhaustive()
    }
}

/// An owned registration for an awaited lifecycle observer.
///
/// Dropping this value removes the observer.  The removal affects events that
/// have not yet begun observer delivery; an observer snapshot already being
/// delivered remains stable for that event.  This makes unsubscribe from an
/// observer callback safe and deterministic without holding the registry lock
/// across an awaited callback.
#[must_use = "drop the subscription to unsubscribe, or retain it for the desired observation lifetime"]
pub struct ObserverSubscription {
    agent: std::sync::Weak<AgentInner>,
    id: u64,
}

impl std::fmt::Debug for ObserverSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObserverSubscription")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for ObserverSubscription {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.upgrade() {
            let mut observers = agent.observers.lock().expect("observer mutex poisoned");
            observers.retain(|registration| registration.id != self.id);
        }
    }
}

/// One observer retained by the agent.
#[derive(Clone)]
pub(crate) struct ObserverRegistration {
    pub(crate) id: u64,
    pub(crate) observer: Arc<dyn EventObserver>,
}

/// A bounded, non-blocking lifecycle-event subscription.
///
/// Unlike [`ObserverSubscription`], receiving events from this subscription
/// never keeps an agent run active. A full queue drops the new event and
/// increments [`Self::dropped_events`], preserving source order for events
/// that are retained without creating backpressure in the run loop.
#[must_use = "drop the subscription to stop receiving events"]
pub struct EventSubscription {
    agent: std::sync::Weak<AgentInner>,
    id: u64,
    receiver: Receiver<crate::event::AgentEvent>,
    dropped: Arc<AtomicU64>,
}

impl std::fmt::Debug for EventSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSubscription")
            .field("id", &self.id)
            .field("dropped_events", &self.dropped_events())
            .finish_non_exhaustive()
    }
}

impl EventSubscription {
    /// Return the next queued event without waiting.
    pub fn try_recv(&self) -> Result<crate::event::AgentEvent, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Number of events discarded because this subscription's queue was full.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.upgrade() {
            let mut subscribers = agent.subscribers.lock().expect("subscriber mutex poisoned");
            subscribers.retain(|registration| registration.id != self.id);
        }
    }
}

/// A lossless, unbounded lifecycle-event subscription.
///
/// Unlike [`EventSubscription`], this subscription never drops an event because
/// of queue capacity. Events are enqueued in the core's sequence order and
/// publishing does not wait for the receiver to drain them or for an executor
/// task to run. The queue is intentionally unbounded: unread events consume
/// caller-owned memory until they are drained or this subscription is dropped.
/// Dropping the subscription releases the receiver and unregisters it from the
/// agent; subsequent sends are harmless.
#[must_use = "drop the subscription to stop receiving events"]
pub struct LosslessEventSubscription {
    agent: std::sync::Weak<AgentInner>,
    id: u64,
    receiver: Receiver<crate::event::AgentEvent>,
}

impl std::fmt::Debug for LosslessEventSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LosslessEventSubscription")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl LosslessEventSubscription {
    /// Return the next queued event without waiting.
    pub fn try_recv(&self) -> Result<crate::event::AgentEvent, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for LosslessEventSubscription {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.upgrade() {
            let mut subscribers = agent
                .lossless_subscribers
                .lock()
                .expect("lossless subscriber mutex poisoned");
            subscribers.retain(|registration| registration.id != self.id);
        }
    }
}

/// One bounded non-blocking event subscription retained by the agent.
#[derive(Clone)]
pub(crate) struct SubscriberRegistration {
    pub(crate) id: u64,
    pub(crate) sender: SyncSender<crate::event::AgentEvent>,
    pub(crate) dropped: Arc<AtomicU64>,
}

/// One unbounded lossless event subscription retained by the agent.
#[derive(Clone)]
pub(crate) struct LosslessSubscriberRegistration {
    pub(crate) id: u64,
    pub(crate) sender: Sender<crate::event::AgentEvent>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl Agent {
    /// Start configuration with an empty profile and no provider.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Return an owned state snapshot.
    pub fn snapshot(&self) -> AgentSnapshot {
        self.inner
            .state
            .lock()
            .expect("agent state mutex poisoned")
            .snapshot()
    }

    /// Return prompt definitions for the currently registered capabilities.
    pub fn tool_definitions(&self) -> Vec<crate::tool::ToolDefinition> {
        self.configuration_snapshot().tools.definitions()
    }

    /// Whether an explicit model provider was configured.
    pub fn has_model_provider(&self) -> bool {
        self.inner
            .provider
            .read()
            .expect("agent provider lock poisoned")
            .is_some()
    }

    /// Atomically replace the configured model identity and provider while idle.
    ///
    /// The replacement preserves the retained linear conversation, tools,
    /// prompts, and explicit queues. A run owns its model/provider pair until
    /// terminal settlement, so this operation rejects active and cancelling
    /// agents rather than changing a provider beneath live model or tool work.
    /// The caller constructs the provider explicitly and is responsible for
    /// validating any provider-specific credential/configuration invariants
    /// before calling this operation.
    pub fn replace_model_provider(
        &self,
        model: ModelDescriptor,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<(), CoreError> {
        let mut provider_slot = self
            .inner
            .provider
            .write()
            .expect("agent provider lock poisoned");
        let mut state = self.inner.state.lock().expect("agent state mutex poisoned");
        match state.phase {
            AgentPhase::Idle => {
                state.model = Some(model);
                *provider_slot = Some(provider);
                Ok(())
            }
            AgentPhase::Running(run_id) | AgentPhase::Cancelling(run_id) => {
                Err(CoreError::ActiveRun { run_id })
            }
        }
    }

    /// Replace the reasoning level used by future runs while the agent is idle.
    pub fn replace_thinking_level(
        &self,
        thinking_level: crate::state::ThinkingLevel,
    ) -> Result<(), CoreError> {
        let mut state = self.inner.state.lock().expect("agent state mutex poisoned");
        match state.phase {
            AgentPhase::Idle => {
                state.thinking_level = thinking_level;
                Ok(())
            }
            AgentPhase::Running(run_id) | AgentPhase::Cancelling(run_id) => {
                Err(CoreError::ActiveRun { run_id })
            }
        }
    }

    /// Atomically replace the prompt, tools, and hooks used by future runs while idle.
    ///
    /// The retained transcript, selected model/provider, reasoning level, and
    /// explicit queues are unchanged. Active and cancelling agents reject the
    /// replacement with [`CoreError::ActiveRun`], leaving their live run's
    /// immutable configuration untouched.
    pub fn replace_configuration(
        &self,
        configuration: AgentConfiguration,
    ) -> Result<(), CoreError> {
        let mut state = self.inner.state.lock().expect("agent state mutex poisoned");
        if let AgentPhase::Running(run_id) | AgentPhase::Cancelling(run_id) = state.phase {
            return Err(CoreError::ActiveRun { run_id });
        }
        let mut current = self
            .inner
            .configuration
            .write()
            .expect("agent configuration lock poisoned");
        state.system_prompt = configuration.system_prompt.clone();
        *current = Arc::new(configuration);
        Ok(())
    }

    /// Restore a validated linear conversation while the agent is idle.
    ///
    /// This is an explicit host boundary for resuming a persisted conversation. The core does
    /// not read files or choose a persistence format; callers provide owned messages and the
    /// same message/tool relationship validation used by compaction. Transient execution state,
    /// queues, and provider accounting are cleared so a resumed conversation starts a fresh run.
    pub fn restore_messages(
        &self,
        messages: Vec<crate::state::AgentMessage>,
    ) -> Result<(), CoreError> {
        let mut state = self.inner.state.lock().expect("agent state mutex poisoned");
        match state.phase {
            AgentPhase::Idle => {
                Self::validate_messages(&messages)?;
                state.replace_messages(messages);
                state.partial_response = None;
                state.is_streaming = false;
                state.pending_tool_calls.clear();
                state.last_error = None;
                state.accounting = crate::state::ModelAccountingSnapshot::default();
                drop(state);
                self.clear_all_queues();
                Ok(())
            }
            AgentPhase::Running(run_id) | AgentPhase::Cancelling(run_id) => {
                Err(CoreError::ActiveRun { run_id })
            }
        }
    }

    /// Restore a validated transcript whose final assistant tool batch still
    /// requires recovery.
    ///
    /// Ordinary [`Self::restore_messages`] rejects an unmatched tool call so a
    /// caller cannot accidentally present an invalid context as settled. A
    /// durable supervisor that has an explicit recovery plan instead uses this
    /// narrow method and must immediately start the exact matching
    /// [`Self::start_recover_tool_calls`] path. The final assistant turn may
    /// be followed by a source-order prefix of already committed tool results;
    /// only its remaining suffix is eligible for recovery. Every preceding
    /// relationship is validated by the normal transcript validator.
    pub fn restore_pending_tool_calls(
        &self,
        messages: Vec<crate::state::AgentMessage>,
        tool_calls: Vec<crate::state::AgentToolCall>,
    ) -> Result<(), CoreError> {
        if tool_calls.is_empty() {
            return Err(CoreError::InvalidTransition(
                crate::error::StateTransitionError::new(
                    "agent",
                    "empty-tool-batch",
                    "restore_pending_tool_calls",
                ),
            ));
        }
        let _ = validate_recovery_tool_batch(&messages, &tool_calls, "restore_pending_tool_calls")?;

        let mut state = self.inner.state.lock().expect("agent state mutex poisoned");
        match state.phase {
            AgentPhase::Idle => {
                state.replace_messages(messages);
                state.partial_response = None;
                state.is_streaming = false;
                state.pending_tool_calls.clear();
                state.last_error = None;
                state.accounting = crate::state::ModelAccountingSnapshot::default();
                drop(state);
                self.clear_all_queues();
                Ok(())
            }
            AgentPhase::Running(run_id) | AgentPhase::Cancelling(run_id) => {
                Err(CoreError::ActiveRun { run_id })
            }
        }
    }

    /// Validate a persisted message vector without changing agent state.
    pub fn validate_messages(messages: &[crate::state::AgentMessage]) -> Result<(), CoreError> {
        crate::compaction::validate_messages(messages).map_err(CoreError::Compaction)
    }

    /// Return an owned copy of the prompt, tools, and hooks used by future runs.
    pub fn configuration(&self) -> AgentConfiguration {
        (*self.configuration_snapshot()).clone()
    }

    /// Replace the automatic-compaction policy while the agent is idle.
    ///
    /// Hosts that select a model after constructing an agent can install the
    /// model's explicit context capacity without rebuilding the conversation.
    /// The policy still requires a caller-owned compactor when enabled.
    pub fn replace_automatic_compaction(
        &self,
        policy: crate::compaction::AutomaticCompactionPolicy,
    ) -> Result<(), CoreError> {
        policy
            .validate()
            .map_err(|message| CoreError::InvalidAutomaticCompactionPolicy {
                message: message.into(),
            })?;
        let state = self.inner.state.lock().expect("agent state mutex poisoned");
        if let AgentPhase::Running(run_id) | AgentPhase::Cancelling(run_id) = state.phase {
            return Err(CoreError::ActiveRun { run_id });
        }
        *self
            .inner
            .automatic_compaction
            .write()
            .expect("automatic compaction policy lock poisoned") = policy;
        Ok(())
    }

    /// Return the currently installed automatic-compaction policy.
    pub fn automatic_compaction(&self) -> crate::compaction::AutomaticCompactionPolicy {
        self.inner
            .automatic_compaction
            .read()
            .expect("automatic compaction policy lock poisoned")
            .clone()
    }

    /// Clone the host policy handle used at the run-loop boundary.
    pub fn hooks(&self) -> Arc<dyn HookSet> {
        Arc::clone(&self.configuration_snapshot().hooks)
    }

    /// Register an awaited lifecycle observer.
    ///
    /// Observers are invoked in registration order for every future event and
    /// are awaited as part of the run.  Keep the returned subscription alive
    /// for as long as observation is wanted; dropping it unsubscribes.  A
    /// registration made from an observer callback begins with the next event.
    pub fn subscribe(&self, observer: Arc<dyn EventObserver>) -> ObserverSubscription {
        let id = self
            .inner
            .next_observer_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.inner
            .observers
            .lock()
            .expect("observer mutex poisoned")
            .push(ObserverRegistration { id, observer });
        ObserverSubscription {
            agent: Arc::downgrade(&self.inner),
            id,
        }
    }

    /// Subscribe to a bounded, non-blocking copy of future lifecycle events.
    ///
    /// This is separate from [`Self::subscribe`]. Events are sent after
    /// awaited observer delivery with a bounded `try_send`; a slow consumer
    /// can neither delay settlement nor cause a background task. When the
    /// queue is full, the new event is dropped and
    /// [`EventSubscription::dropped_events`] records it.
    pub fn subscribe_nonblocking(&self, capacity: std::num::NonZeroUsize) -> EventSubscription {
        let id = self
            .inner
            .next_subscriber_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let (sender, receiver) = sync_channel(capacity.get());
        let dropped = Arc::new(AtomicU64::new(0));
        self.inner
            .subscribers
            .lock()
            .expect("subscriber mutex poisoned")
            .push(SubscriberRegistration {
                id,
                sender,
                dropped: Arc::clone(&dropped),
            });
        EventSubscription {
            agent: Arc::downgrade(&self.inner),
            id,
            receiver,
            dropped,
        }
    }

    /// Subscribe to an unbounded, lossless copy of future lifecycle events.
    ///
    /// This path is separate from [`Self::subscribe_nonblocking`]. Every event
    /// is sent in sequence order while the receiver is alive; no bounded
    /// overflow or hidden lossy fallback exists. The unbounded queue is owned
    /// by the caller, so a receiver that is not drained retains every event and
    /// can grow without limit. Dropping the returned subscription releases that
    /// queued memory, unregisters the receiver, and never delays run settlement.
    pub fn subscribe_lossless(&self) -> LosslessEventSubscription {
        let id = self
            .inner
            .next_lossless_subscriber_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let (sender, receiver) = channel();
        self.inner
            .lossless_subscribers
            .lock()
            .expect("lossless subscriber mutex poisoned")
            .push(LosslessSubscriberRegistration { id, sender });
        LosslessEventSubscription {
            agent: Arc::downgrade(&self.inner),
            id,
            receiver,
        }
    }

    /// Resolve after the active run has fully settled and the agent is idle.
    ///
    /// In particular, awaited observers for the terminal `AgentEnd` event
    /// run before this future resolves.
    pub async fn wait_for_idle(&self) {
        std::future::poll_fn(|context| {
            if self.is_idle() {
                return Poll::Ready(());
            }
            self.inner.idle_notifier.register(context.waker());
            if self.is_idle() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }

    /// Start a prompt run.  No model work is performed until the caller drives the returned
    /// handle on its own executor.
    pub fn start_prompt(&self, prompt: impl Into<String>) -> Result<RunHandle, CoreError> {
        self.start_run(vec![prompt.into()], false)
    }

    /// Continue from a retained user or tool-result message without adding a new prompt.
    ///
    /// If the transcript ends in an assistant message, Pi permits continuation only by consuming
    /// queued steering first, then queued follow-up input. Those consumed messages become the
    /// next run's prompt events; remaining steering is deliberately deferred until after its
    /// first assistant turn.
    pub fn start_continue(&self) -> Result<RunHandle, CoreError> {
        let assistant_tail = {
            let state = self.inner.state.lock().expect("agent state mutex poisoned");
            match state.phase {
                AgentPhase::Running(run_id) | AgentPhase::Cancelling(run_id) => {
                    return Err(CoreError::ActiveRun { run_id });
                }
                AgentPhase::Idle => {}
            }
            match state.messages.last() {
                None => {
                    return Err(CoreError::InvalidTransition(
                        crate::error::StateTransitionError::new("agent", "empty", "continue"),
                    ));
                }
                Some(AgentMessage::Assistant { .. }) => true,
                Some(AgentMessage::User { .. } | AgentMessage::ToolResult { .. }) => false,
            }
        };
        if !assistant_tail {
            return self.start_run(Vec::new(), false);
        }

        let queued = self.drain_continue_tail_messages();
        if queued.is_empty() {
            return Err(CoreError::InvalidTransition(
                crate::error::StateTransitionError::new("agent", "assistant-tail", "continue"),
            ));
        }
        self.start_run(
            queued.into_iter().map(|message| message.content).collect(),
            true,
        )
    }

    /// Resume the unresolved suffix of a retained terminal assistant tool batch.
    ///
    /// A durable supervisor uses this after restoring a validated transcript
    /// whose last assistant response requested tools. The regular core tool
    /// scheduler, hooks, cancellation, result insertion, and subsequent model
    /// continuation remain exactly the same as a fresh run; this method only
    /// selects the first scheduler step rather than issuing another provider
    /// request before those calls are resolved.
    pub fn start_recover_tool_calls(
        &self,
        tool_calls: Vec<crate::state::AgentToolCall>,
    ) -> Result<RunHandle, CoreError> {
        if tool_calls.is_empty() {
            return Err(CoreError::InvalidTransition(
                crate::error::StateTransitionError::new(
                    "agent",
                    "empty-tool-batch",
                    "recover_tool_calls",
                ),
            ));
        }
        let prior_all_terminate = {
            let state = self.inner.state.lock().expect("agent state mutex poisoned");
            match state.phase {
                AgentPhase::Running(run_id) | AgentPhase::Cancelling(run_id) => {
                    return Err(CoreError::ActiveRun { run_id });
                }
                AgentPhase::Idle => {}
            }
            validate_recovery_tool_batch(&state.messages, &tool_calls, "recover_tool_calls")?
        };
        let mut run = self.start_run(Vec::new(), false)?;
        run.recovery_tool_calls = Some(tool_calls);
        run.recovery_prior_all_terminate = Some(prior_all_terminate);
        Ok(run)
    }

    fn drain_continue_tail_messages(&self) -> Vec<QueuedMessage> {
        let steering_mode = self.steering_mode();
        let follow_up_mode = self.follow_up_mode();
        let mut queues = self
            .inner
            .queues
            .lock()
            .expect("agent queue mutex poisoned");
        let steering = queues.steering.drain(steering_mode);
        if steering.is_empty() {
            queues.follow_up.drain(follow_up_mode)
        } else {
            steering
        }
    }

    fn start_run(
        &self,
        initial_contents: Vec<String>,
        skip_initial_steering: bool,
    ) -> Result<RunHandle, CoreError> {
        let run_number = self
            .inner
            .next_run_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let run_id = crate::state::RunId(run_number);
        let mut state = self.inner.state.lock().expect("agent state mutex poisoned");
        if let AgentPhase::Running(active) | AgentPhase::Cancelling(active) = state.phase {
            return Err(CoreError::ActiveRun { run_id: active });
        }
        let message_start_index = state.messages.len();
        let initial_messages = initial_contents
            .into_iter()
            .map(|content| AgentMessage::User {
                id: state.allocate_message_id(),
                content,
            })
            .collect::<Vec<_>>();
        for message in &initial_messages {
            state.append_message(message.clone());
        }
        state.phase = AgentPhase::Running(run_id);
        state.last_error = None;
        state.partial_response = None;
        state.is_streaming = false;
        state.pending_tool_calls.clear();
        let configuration = Arc::clone(
            &*self
                .inner
                .configuration
                .read()
                .expect("agent configuration lock poisoned"),
        );
        drop(state);
        let run_state = Arc::new(Mutex::new(crate::state::RunState::created(run_id)));
        let cancellation = CancellationToken::new();
        *self
            .inner
            .active_run
            .lock()
            .expect("active run mutex poisoned") = Some(ActiveRun {
            id: run_id,
            state: Arc::clone(&run_state),
            cancellation: cancellation.clone(),
        });
        Ok(RunHandle {
            agent: Arc::downgrade(&self.inner),
            state: run_state,
            cancellation,
            initial_messages,
            message_start_index,
            skip_initial_steering,
            configuration,
            policy: Mutex::new(crate::run::RunPolicyState::default()),
            next_effect_id: AtomicU64::new(0),
            recovery_tool_calls: None,
            recovery_prior_all_terminate: None,
        })
    }

    fn configuration_snapshot(&self) -> Arc<AgentConfiguration> {
        let _state = self.inner.state.lock().expect("agent state mutex poisoned");
        Arc::clone(
            &*self
                .inner
                .configuration
                .read()
                .expect("agent configuration lock poisoned"),
        )
    }

    /// Queue steering input for the next eligible active-turn drain point.
    ///
    /// Pi permits queuing while idle as well as while a run is active. Idle input is consumed by
    /// the next prompt/continuation run rather than implicitly starting work.
    pub fn enqueue_steering(&self, content: impl Into<String>) -> Result<u64, CoreError> {
        Ok(self
            .inner
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .steering
            .push(content))
    }

    /// Queue steering input using upstream Pi's `steer` vocabulary.
    pub fn steer(&self, content: impl Into<String>) -> Result<u64, CoreError> {
        self.enqueue_steering(content)
    }

    /// Queue follow-up input for the next idle boundary of a run.
    ///
    /// Idle input waits for an explicit prompt or continuation; queuing is never an implicit run.
    pub fn enqueue_follow_up(&self, content: impl Into<String>) -> Result<u64, CoreError> {
        Ok(self
            .inner
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .follow_up
            .push(content))
    }

    /// Queue follow-up input using upstream Pi's `followUp` vocabulary.
    pub fn follow_up(&self, content: impl Into<String>) -> Result<u64, CoreError> {
        self.enqueue_follow_up(content)
    }

    /// Remove queued steering messages without changing conversation history.
    pub fn clear_steering_queue(&self) {
        self.inner
            .queues
            .lock()
            .expect("agent queue mutex poisoned")
            .steering
            .clear();
    }

    /// Remove queued follow-up messages without changing conversation history.
    pub fn clear_follow_up_queue(&self) {
        self.inner
            .queues
            .lock()
            .expect("agent queue mutex poisoned")
            .follow_up
            .clear();
    }

    /// Remove all queued messages without changing conversation history.
    pub fn clear_all_queues(&self) {
        self.inner
            .queues
            .lock()
            .expect("agent queue mutex poisoned")
            .clear();
    }

    /// Whether either explicit queue contains input waiting for an eligible drain point.
    pub fn has_queued_messages(&self) -> bool {
        let queues = self
            .inner
            .queues
            .lock()
            .expect("agent queue mutex poisoned");
        !queues.steering.is_empty() || !queues.follow_up.is_empty()
    }

    /// Return an owned inspection view of the two core-owned prompt queues.
    ///
    /// The snapshot is presentation-safe: mutating it never changes the agent. Hosts use this
    /// rather than maintaining a shadow queue so displayed prompts disappear only when the run
    /// loop drains the corresponding core queue.
    pub fn queue_snapshot(&self) -> AgentQueues {
        self.inner
            .queues
            .lock()
            .expect("agent queue mutex poisoned")
            .clone()
    }

    /// Change the steering drain mode for subsequent eligible turn boundaries.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        *self
            .inner
            .steering_mode
            .lock()
            .expect("agent steering mode mutex poisoned") = mode;
    }

    /// Return the currently configured steering drain mode.
    pub fn steering_mode(&self) -> QueueMode {
        *self
            .inner
            .steering_mode
            .lock()
            .expect("agent steering mode mutex poisoned")
    }

    /// Change the follow-up drain mode for subsequent idle boundaries.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        *self
            .inner
            .follow_up_mode
            .lock()
            .expect("agent follow-up mode mutex poisoned") = mode;
    }

    /// Return the currently configured follow-up drain mode.
    pub fn follow_up_mode(&self) -> QueueMode {
        *self
            .inner
            .follow_up_mode
            .lock()
            .expect("agent follow-up mode mutex poisoned")
    }

    /// Clear transcript, transient state, last error, and explicit queues while idle.
    ///
    /// Reset never starts, cancels, or otherwise settles a run. Calling it while a run owns the
    /// agent is rejected so durable state cannot change beneath model/tool work.
    pub fn reset(&self) -> Result<(), CoreError> {
        let mut state = self.inner.state.lock().expect("agent state mutex poisoned");
        match state.phase {
            AgentPhase::Idle => {
                state.messages.clear();
                state.host_messages.clear();
                state.partial_response = None;
                state.is_streaming = false;
                state.pending_tool_calls.clear();
                state.last_error = None;
                state.accounting = crate::state::ModelAccountingSnapshot::default();
                drop(state);
                self.clear_all_queues();
                Ok(())
            }
            AgentPhase::Running(run_id) | AgentPhase::Cancelling(run_id) => {
                Err(CoreError::ActiveRun { run_id })
            }
        }
    }

    /// Request cancellation for the active run, if one exists.
    ///
    /// An already-driving run remains active until its cancellation-aware
    /// model/tool boundary emits terminal events and settles observers. A run
    /// that has not yet been driven has no such boundary, so it settles here.
    /// No active run is not an error.
    pub fn abort(&self) {
        let active = self
            .inner
            .active_run
            .lock()
            .expect("active run mutex poisoned")
            .clone();
        if let Some(active) = active {
            active.cancellation.cancel();
            let mut run_state = active.state.lock().expect("run state mutex poisoned");
            let settle_immediately = run_state.phase == crate::state::RunPhase::Created;
            if settle_immediately {
                run_state.phase = crate::state::RunPhase::Cancelled;
                run_state.stop_reason = Some(crate::state::StopReason::Cancelled);
            }
            drop(run_state);
            let mut state = self.inner.state.lock().expect("agent state mutex poisoned");
            state.partial_response = None;
            state.is_streaming = false;
            state.pending_tool_calls.clear();
            if settle_immediately {
                state.phase = AgentPhase::Idle;
            } else if !matches!(state.phase, AgentPhase::Idle) {
                state.phase = AgentPhase::Cancelling(active.id);
            }
            drop(state);
            self.inner
                .queues
                .lock()
                .expect("queue mutex poisoned")
                .clear();
            if settle_immediately {
                let mut active_slot = self
                    .inner
                    .active_run
                    .lock()
                    .expect("active run mutex poisoned");
                if active_slot
                    .as_ref()
                    .is_some_and(|current| current.id == active.id)
                {
                    active_slot.take();
                }
                drop(active_slot);
                self.inner.idle_notifier.notify();
            }
        }
    }

    fn is_idle(&self) -> bool {
        matches!(
            self.inner
                .state
                .lock()
                .expect("agent state mutex poisoned")
                .phase,
            AgentPhase::Idle
        )
    }
}

/// Validate the one recovery shape that permits an intentionally unfinished
/// tool batch. The core normally rejects every unmatched assistant call. A
/// durable supervisor is allowed to retain a final assistant response plus a
/// committed source-order result prefix and execute only its missing suffix.
///
/// This helper derives whether already committed results all requested
/// termination, preserving the normal batch-level termination behavior after
/// the suffix settles.
fn validate_recovery_tool_batch(
    messages: &[AgentMessage],
    recovery_calls: &[crate::state::AgentToolCall],
    operation: &'static str,
) -> Result<bool, CoreError> {
    let Some(assistant_index) = messages
        .iter()
        .rposition(|message| matches!(message, AgentMessage::Assistant { .. }))
    else {
        return Err(recovery_transition("missing-assistant", operation));
    };
    let AgentMessage::Assistant {
        tool_calls,
        stop_reason,
        ..
    } = &messages[assistant_index]
    else {
        unreachable!("assistant index was selected from assistant messages");
    };
    if *stop_reason != Some(crate::state::StopReason::ToolUse) {
        return Err(recovery_transition("assistant-not-tool-use", operation));
    }
    let settled_results = &messages[assistant_index.saturating_add(1)..];
    if settled_results.len() >= tool_calls.len()
        || tool_calls.get(settled_results.len()..) != Some(recovery_calls)
    {
        return Err(recovery_transition("assistant-tool-mismatch", operation));
    }

    let mut prior_all_terminate = true;
    for (call, message) in tool_calls.iter().zip(settled_results) {
        let AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            terminate,
            ..
        } = message
        else {
            return Err(recovery_transition("non-result-after-assistant", operation));
        };
        if tool_call_id != &call.id || tool_name != &call.name {
            return Err(recovery_transition("result-prefix-mismatch", operation));
        }
        prior_all_terminate &= *terminate;
    }

    let mut settled_transcript = messages.to_vec();
    let AgentMessage::Assistant {
        tool_calls: settled_calls,
        ..
    } = &mut settled_transcript[assistant_index]
    else {
        unreachable!("assistant index was selected from assistant messages");
    };
    settled_calls.truncate(settled_results.len());
    Agent::validate_messages(&settled_transcript)?;
    Ok(prior_all_terminate)
}

fn recovery_transition(from: &'static str, operation: &'static str) -> CoreError {
    CoreError::InvalidTransition(crate::error::StateTransitionError::new(
        "agent", from, operation,
    ))
}
