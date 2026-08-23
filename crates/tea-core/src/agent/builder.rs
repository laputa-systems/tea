//! Agent configuration builder.

use super::{Agent, AgentConfiguration, AgentInner, IdleNotifier, ObserverRegistration};
use crate::effect::{EffectGate, NoopEffectGate, RunProvenance};
use crate::event::EventObserver;
use crate::hooks::{HookSet, NoHooks};
use crate::queue::{AgentQueues, QueueMode};
use crate::scheduler::ModelProvider;
use crate::state::{AgentState, ModelDescriptor, ThinkingLevel};
use crate::tool::{AgentTool, ToolRegistry};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};

/// Configuration builder for an [`Agent`].
#[derive(Default)]
pub struct AgentBuilder {
    system_prompt: String,
    model: Option<ModelDescriptor>,
    thinking_level: ThinkingLevel,
    host_messages: Vec<crate::state::SerializedJson>,
    tools: ToolRegistry,
    provider: Option<Arc<dyn ModelProvider>>,
    compactor: Option<Arc<dyn crate::compaction::Compactor>>,
    automatic_compaction: crate::compaction::AutomaticCompactionPolicy,
    tool_result_projection: crate::tool::ToolResultProjectionPolicy,
    tool_failure_circuit_breaker: crate::tool::ToolFailureCircuitBreaker,
    hooks: Option<Arc<dyn HookSet>>,
    effect_gate: Option<Arc<dyn EffectGate>>,
    provenance: RunProvenance,
    observers: Vec<Arc<dyn EventObserver>>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    prompt_layout_ledger: Option<Arc<crate::measurement::PromptLayoutLedger>>,
}

impl std::fmt::Debug for AgentBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentBuilder")
            .field("system_prompt", &self.system_prompt)
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("tools", &self.tools)
            .finish()
    }
}

impl AgentBuilder {
    /// Set system instructions.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    /// Set provider-independent model identity.
    pub fn model(mut self, model: ModelDescriptor) -> Self {
        self.model = Some(model);
        self
    }

    /// Set reasoning level.
    pub fn thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = level;
        self
    }

    /// Add one explicit host-only context value.
    ///
    /// Host messages are not ambient configuration and are not converted to a
    /// provider request unless the configured context hook chooses to do so.
    pub fn host_message(mut self, message: crate::state::SerializedJson) -> Self {
        self.host_messages.push(message);
        self
    }

    /// Replace the complete executable tool registry.
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    /// Add one executable tool while preserving insertion order.
    pub fn tool(mut self, tool: Arc<dyn AgentTool>) -> Self {
        self.tools.insert(tool);
        self
    }

    /// Remove one named executable capability before building the agent.
    ///
    /// This makes profile composition explicit: callers may start with the
    /// batteries-included set and deliberately omit a capability without
    /// changing its prompt or scheduler implementation behind the scenes.
    pub fn remove_tool(mut self, name: &str) -> Self {
        self.tools.remove(name);
        self
    }

    /// Attach a caller-owned model provider.
    pub fn model_provider(mut self, provider: Arc<dyn ModelProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Attach a caller-owned manual compactor.
    pub fn compactor(mut self, compactor: Arc<dyn crate::compaction::Compactor>) -> Self {
        self.compactor = Some(compactor);
        self
    }

    /// Enable an explicitly configured automatic compaction policy.
    ///
    /// A configured policy still requires [`Self::compactor`]; the core never
    /// invents a summary provider or prompt.
    pub fn automatic_compaction(
        mut self,
        policy: crate::compaction::AutomaticCompactionPolicy,
    ) -> Result<Self, crate::error::CoreError> {
        policy.validate().map_err(|message| {
            crate::error::CoreError::InvalidAutomaticCompactionPolicy {
                message: message.into(),
            }
        })?;
        self.automatic_compaction = policy;
        Ok(self)
    }

    /// Configure bounded model-facing presentation of raw tool results.
    pub fn tool_result_projection(
        mut self,
        policy: crate::tool::ToolResultProjectionPolicy,
    ) -> Result<Self, crate::error::CoreError> {
        policy.validate().map_err(|message| {
            crate::error::CoreError::InvalidToolResultProjectionPolicy {
                message: message.into(),
            }
        })?;
        self.tool_result_projection = policy;
        Ok(self)
    }

    /// Configure the run-local repeated retryable-failure circuit breaker.
    pub fn tool_failure_circuit_breaker(
        mut self,
        policy: crate::tool::ToolFailureCircuitBreaker,
    ) -> Self {
        self.tool_failure_circuit_breaker = policy;
        self
    }

    /// Attach host policy hooks.
    pub fn hooks(mut self, hooks: Arc<dyn HookSet>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Attach a host-owned barrier around provider, tool, and hook effects.
    ///
    /// A durable supervisor uses this to commit effect intent before dispatch
    /// and settlement before the core can continue. The default is an
    /// immediate no-op gate for sessionless embeddings.
    pub fn effect_gate(mut self, effect_gate: Arc<dyn EffectGate>) -> Self {
        self.effect_gate = Some(effect_gate);
        self
    }

    /// Attach opaque durable attribution to each effect of future runs.
    ///
    /// The core stores and forwards this data but never interprets it as a
    /// path, credential, or persistence authority.
    pub fn effect_provenance(mut self, provenance: RunProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// Add an awaited lifecycle observer in registration order.
    pub fn observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    /// Select how steering messages are drained at eligible turn boundaries.
    pub fn steering_mode(mut self, mode: QueueMode) -> Self {
        self.steering_mode = mode;
        self
    }

    /// Select how follow-up messages are drained at the idle boundary.
    pub fn follow_up_mode(mut self, mode: QueueMode) -> Self {
        self.follow_up_mode = mode;
        self
    }

    /// Share a volatile prompt-layout ledger with other agents in one live
    /// host session. The ledger emits only content-free evidence. A host that
    /// shares it directly must serialize provider dispatch in that scope;
    /// [`crate::runtime::SessionRuntime`] already enforces one active operation.
    pub fn prompt_layout_ledger(
        mut self,
        ledger: Arc<crate::measurement::PromptLayoutLedger>,
    ) -> Self {
        self.prompt_layout_ledger = Some(ledger);
        self
    }

    /// Build an owned agent.
    pub fn build(self) -> Agent {
        let next_observer_id = self.observers.len() as u64;
        let mut state = AgentState::default();
        state.system_prompt = self.system_prompt;
        state.model = self.model;
        state.thinking_level = self.thinking_level;
        state.host_messages = self.host_messages;
        let system_prompt = state.system_prompt.clone();
        Agent {
            inner: Arc::new(AgentInner {
                state: Mutex::new(state),
                queues: Mutex::new(AgentQueues::default()),
                active_run: Mutex::new(None),
                configuration: RwLock::new(Arc::new(AgentConfiguration::with_effect_gate(
                    system_prompt,
                    self.tools,
                    self.hooks.unwrap_or_else(|| Arc::new(NoHooks)),
                    self.effect_gate.unwrap_or_else(|| Arc::new(NoopEffectGate)),
                    self.provenance,
                ))),
                steering_mode: Mutex::new(self.steering_mode),
                follow_up_mode: Mutex::new(self.follow_up_mode),
                provider: RwLock::new(self.provider),
                compactor: RwLock::new(self.compactor),
                automatic_compaction: RwLock::new(self.automatic_compaction),
                tool_result_projection: self.tool_result_projection,
                tool_failure_circuit_breaker: self.tool_failure_circuit_breaker,
                prompt_layout_ledger: self
                    .prompt_layout_ledger
                    .unwrap_or_else(|| Arc::new(crate::measurement::PromptLayoutLedger::default())),
                observers: Mutex::new(
                    self.observers
                        .into_iter()
                        .enumerate()
                        .map(|(index, observer)| ObserverRegistration {
                            id: (index as u64).saturating_add(1),
                            observer,
                        })
                        .collect(),
                ),
                next_observer_id: AtomicU64::new(next_observer_id),
                subscribers: Mutex::new(Vec::new()),
                next_subscriber_id: AtomicU64::new(0),
                lossless_subscribers: Mutex::new(Vec::new()),
                next_lossless_subscriber_id: AtomicU64::new(0),
                next_run_id: AtomicU64::new(0),
                idle_notifier: IdleNotifier::default(),
            }),
        }
    }
}
