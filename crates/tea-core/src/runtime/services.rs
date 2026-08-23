//! Host-owned executable services for durable sessions.

use crate::agent::{Agent, AgentConfiguration};
use crate::harness::{HarnessError, ResolvedHarness};
use std::collections::BTreeSet;
use std::sync::Arc;
use tea_core::compaction::{AutomaticCompactionPolicy, Compactor};
use tea_core::effect::{EffectGate, RunProvenance};
use tea_core::hooks::{HookSet, NoHooks};
use tea_core::scheduler::ModelProvider;
use tea_core::state::{ModelDescriptor, ThinkingLevel};
use tea_core::tool::{ToolFailureCircuitBreaker, ToolRegistry, ToolResultProjectionPolicy};
use tea_session::ArtifactPolicy;

/// Host-owned executable authority used to run a resolved harness.
///
/// This retains provider transport, trusted base tools, and host implementations.
/// `ResolvedHarness` deliberately carries none of those capabilities: it is the
/// provider-independent result of resolving one immutable harness revision.
#[derive(Clone)]
pub struct RuntimeServices {
    model: Option<ModelDescriptor>,
    thinking_level: ThinkingLevel,
    trusted_tools: ToolRegistry,
    provider: Arc<dyn ModelProvider>,
    base_hooks: Arc<dyn HookSet>,
    compactor: Option<Arc<dyn Compactor>>,
    automatic_compaction: AutomaticCompactionPolicy,
    tool_result_projection: ToolResultProjectionPolicy,
    tool_failure_circuit_breaker: ToolFailureCircuitBreaker,
    replay_safe_tools: BTreeSet<String>,
    artifact_policy: ArtifactPolicy,
    prompt_layout_ledger: Arc<crate::measurement::PromptLayoutLedger>,
}

impl std::fmt::Debug for RuntimeServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeServices")
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("trusted_tools", &self.trusted_tools)
            .field("has_compactor", &self.compactor.is_some())
            .field("replay_safe_tools", &self.replay_safe_tools)
            .finish_non_exhaustive()
    }
}

impl RuntimeServices {
    /// Construct host services with caller-owned provider and trusted tools.
    pub fn new(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Self {
        Self {
            model: None,
            thinking_level: ThinkingLevel::Off,
            trusted_tools: tools,
            provider,
            base_hooks: Arc::new(NoHooks),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::default(),
            tool_result_projection: ToolResultProjectionPolicy::default(),
            tool_failure_circuit_breaker: ToolFailureCircuitBreaker::default(),
            replay_safe_tools: BTreeSet::new(),
            artifact_policy: ArtifactPolicy::default(),
            prompt_layout_ledger: Arc::new(crate::measurement::PromptLayoutLedger::default()),
        }
    }

    /// Convert a trusted host configuration into host runtime services.
    /// The configuration's existing effect gate and provenance are intentionally
    /// discarded: each durable epoch installs its own gate and exact
    /// operation/epoch attribution at construction time.
    pub fn from_agent_configuration(
        provider: Arc<dyn ModelProvider>,
        configuration: AgentConfiguration,
    ) -> Self {
        Self::new(provider, configuration.tools).hooks(configuration.hooks)
    }

    /// Set the provider-independent model identity.
    pub fn model(mut self, model: ModelDescriptor) -> Self {
        self.model = Some(model);
        self
    }

    /// Set the default thinking level.
    pub fn thinking_level(mut self, thinking_level: ThinkingLevel) -> Self {
        self.thinking_level = thinking_level;
        self
    }

    /// Return the default reasoning level selected by the host.
    pub fn thinking_level_value(&self) -> ThinkingLevel {
        self.thinking_level
    }

    /// Install host policy hooks to be wrapped by resolved extension hooks.
    pub fn hooks(mut self, hooks: Arc<dyn HookSet>) -> Self {
        self.base_hooks = hooks;
        self
    }

    /// Install a caller-owned compactor without giving the harness a provider implementation.
    pub fn compactor(mut self, compactor: Arc<dyn Compactor>) -> Self {
        self.compactor = Some(compactor);
        self
    }

    /// Set the explicit automatic-compaction policy.
    pub fn automatic_compaction(mut self, policy: AutomaticCompactionPolicy) -> Self {
        self.automatic_compaction = policy;
        self
    }

    /// Set bounded model-facing tool-result projection policy.
    pub fn tool_result_projection(mut self, policy: ToolResultProjectionPolicy) -> Self {
        self.tool_result_projection = policy;
        self
    }

    /// Set the per-run repeated tool-failure circuit breaker policy.
    pub fn tool_failure_circuit_breaker(mut self, policy: ToolFailureCircuitBreaker) -> Self {
        self.tool_failure_circuit_breaker = policy;
        self
    }

    /// Share prompt-layout continuity across fresh agents built by this live
    /// runtime. The ledger is volatile and emits only content-free evidence.
    pub fn prompt_layout_ledger(
        mut self,
        ledger: Arc<crate::measurement::PromptLayoutLedger>,
    ) -> Self {
        self.prompt_layout_ledger = ledger;
        self
    }

    /// Select an opaque equality-only serving/cache scope for this live
    /// runtime's layout ledger.
    pub fn prompt_cache_scope(mut self, scope: crate::measurement::PromptCacheScope) -> Self {
        let policy = self.prompt_layout_ledger.policy_value();
        self.prompt_layout_ledger =
            Arc::new(crate::measurement::PromptLayoutLedger::new(scope).policy(policy));
        self
    }

    /// Select whether layout continuity is observed or rejected before
    /// provider dispatch.
    pub fn prompt_layout_policy(mut self, policy: crate::measurement::PromptLayoutPolicy) -> Self {
        let scope = self.prompt_layout_ledger.scope();
        self.prompt_layout_ledger =
            Arc::new(crate::measurement::PromptLayoutLedger::new(scope).policy(policy));
        self
    }

    /// Explicitly allow replay only for a named host-owned tool.
    ///
    /// This is a host configuration capability, not a model or plugin claim.
    /// The supervisor verifies that the named executable capability exists
    /// before beginning an effect with this declaration.
    pub fn replay_safe_tool(mut self, name: impl Into<String>) -> Self {
        self.replay_safe_tools.insert(name.into());
        self
    }

    /// Set the immutable model-readable artifact policy for this snapshot.
    ///
    /// When `redact_before_persist` is enabled, the caller's installed
    /// `after_tool` policy is responsible for producing the redacted result
    /// before it reaches the durable supervisor.
    pub fn artifact_policy(mut self, policy: ArtifactPolicy) -> Result<Self, HarnessError> {
        policy
            .validate()
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        self.artifact_policy = policy;
        Ok(self)
    }

    /// Borrow the host's trusted base tools.
    pub(crate) fn trusted_tools(&self) -> &ToolRegistry {
        &self.trusted_tools
    }

    /// Clone the host hook set so a resolved snapshot can wrap it with freshly
    /// compiled, source-pinned extension hooks.
    pub(crate) fn base_hook_set(&self) -> Arc<dyn HookSet> {
        Arc::clone(&self.base_hooks)
    }

    pub(crate) fn automatic_compaction_policy(&self) -> &AutomaticCompactionPolicy {
        &self.automatic_compaction
    }

    pub(crate) fn tool_result_projection_policy(&self) -> &ToolResultProjectionPolicy {
        &self.tool_result_projection
    }

    pub(crate) fn tool_failure_circuit_breaker_policy(&self) -> ToolFailureCircuitBreaker {
        self.tool_failure_circuit_breaker
    }

    pub(crate) fn prompt_layout_scope(&self) -> crate::measurement::PromptCacheScope {
        self.prompt_layout_ledger.scope()
    }

    pub(crate) fn prompt_layout_policy_value(&self) -> crate::measurement::PromptLayoutPolicy {
        self.prompt_layout_ledger.policy_value()
    }

    pub(crate) fn replay_safe_tools(&self) -> &BTreeSet<String> {
        &self.replay_safe_tools
    }

    pub(crate) fn artifact_policy_config(&self) -> &ArtifactPolicy {
        &self.artifact_policy
    }

    /// Build one immutable core epoch from a resolved harness and stable host
    /// tools. The resolved harness contributes only provider-independent
    /// configuration; provider transport and trusted base tools stay here.
    ///
    /// The caller must reject name collisions before this point; replacing an
    /// editable tool would silently change the frozen snapshot contract.
    pub(crate) fn build_agent_with_tools(
        &self,
        resolved: &ResolvedHarness,
        effect_gate: Arc<dyn EffectGate>,
        provenance: RunProvenance,
        additional_tools: ToolRegistry,
    ) -> Result<Agent, HarnessError> {
        let mut tools = self.trusted_tools.clone();
        for name in resolved
            .extension_tools()
            .names()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        {
            if tools.get(&name).is_some() {
                return Err(HarnessError::invalid_state(format!(
                    "resolved extension tool {name} collides with a trusted host capability",
                )));
            }
            let tool = resolved
                .extension_tools()
                .get(&name)
                .expect("resolved extension tool remains registered")
                .clone();
            tools.insert(tool);
        }
        let names = additional_tools
            .names()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for name in names {
            let tool = additional_tools
                .get(&name)
                .expect("registered additional tool remains present")
                .clone();
            if tools.get(&name).is_some() {
                return Err(HarnessError::invalid_state(format!(
                    "immutable template collides with reserved host tool {name}"
                )));
            }
            tools.insert(tool);
        }
        let mut builder = Agent::builder()
            .system_prompt(resolved.system_prompt().to_owned())
            .tools(tools)
            .model_provider(Arc::clone(&self.provider))
            .hooks(resolved.hooks())
            .effect_gate(effect_gate)
            .effect_provenance(provenance)
            .thinking_level(self.thinking_level)
            .tool_failure_circuit_breaker(resolved.tool_failure_circuit_breaker());
        builder = builder.prompt_layout_ledger(Arc::clone(&self.prompt_layout_ledger));
        if let Some(model) = &self.model {
            builder = builder.model(model.clone());
        }
        if let Some(compactor) = &self.compactor {
            builder = builder.compactor(Arc::clone(compactor));
        }
        builder = builder
            .automatic_compaction(resolved.automatic_compaction_policy().clone())?
            .tool_result_projection(resolved.tool_result_projection_policy().clone())?;
        Ok(builder.build())
    }
}
