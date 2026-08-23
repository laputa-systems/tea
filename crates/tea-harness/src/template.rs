//! Immutable core configuration used to construct one durable epoch.

use crate::HarnessError;
use std::collections::BTreeSet;
use std::sync::Arc;
use tea_core::agent::Agent;
use tea_core::compaction::{AutomaticCompactionPolicy, Compactor};
use tea_core::effect::{EffectGate, RunProvenance};
use tea_core::hooks::{HookSet, NoHooks};
use tea_core::scheduler::ModelProvider;
use tea_core::state::{ModelDescriptor, ThinkingLevel};
use tea_core::tool::{ToolFailureCircuitBreaker, ToolRegistry, ToolResultProjectionPolicy};
use tea_session::ArtifactPolicy;

/// Immutable core capabilities resolved for one durable harness snapshot.
///
/// A real harness revision owns the source and validation behind these values;
/// this type intentionally retains only the executable core boundary. It is
/// cloned into each epoch so an active core run never observes an in-place
/// configuration replacement.
#[derive(Clone)]
pub struct CoreEpochTemplate {
    system_prompt: String,
    model: Option<ModelDescriptor>,
    thinking_level: ThinkingLevel,
    tools: ToolRegistry,
    provider: Arc<dyn ModelProvider>,
    hooks: Arc<dyn HookSet>,
    compactor: Option<Arc<dyn Compactor>>,
    automatic_compaction: AutomaticCompactionPolicy,
    tool_result_projection: ToolResultProjectionPolicy,
    tool_failure_circuit_breaker: ToolFailureCircuitBreaker,
    replay_safe_tools: BTreeSet<String>,
    artifact_policy: ArtifactPolicy,
}

impl std::fmt::Debug for CoreEpochTemplate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CoreEpochTemplate")
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("tools", &self.tools)
            .field("has_compactor", &self.compactor.is_some())
            .field("replay_safe_tools", &self.replay_safe_tools)
            .finish_non_exhaustive()
    }
}

impl CoreEpochTemplate {
    /// Construct an epoch template with caller-owned provider and tools.
    pub fn new(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Self {
        Self {
            system_prompt: String::new(),
            model: None,
            thinking_level: ThinkingLevel::Off,
            tools,
            provider,
            hooks: Arc::new(NoHooks),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::default(),
            tool_result_projection: ToolResultProjectionPolicy::default(),
            tool_failure_circuit_breaker: ToolFailureCircuitBreaker::default(),
            replay_safe_tools: BTreeSet::new(),
            artifact_policy: ArtifactPolicy::default(),
        }
    }

    /// Convert a trusted host configuration into an immutable durable epoch
    /// template. The configuration's existing effect gate and provenance are
    /// intentionally discarded: each durable epoch installs its own gate and
    /// exact operation/epoch attribution at construction time.
    pub fn from_agent_configuration(
        provider: Arc<dyn ModelProvider>,
        configuration: tea_core::AgentConfiguration,
    ) -> Self {
        Self::new(provider, configuration.tools)
            .system_prompt(configuration.system_prompt)
            .hooks(configuration.hooks)
    }

    /// Set immutable system instructions for each epoch built from this template.
    pub fn system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
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

    /// Install immutable host policy hooks for this snapshot.
    pub fn hooks(mut self, hooks: Arc<dyn HookSet>) -> Self {
        self.hooks = hooks;
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

    pub(crate) fn is_replay_safe(&self, name: &str) -> bool {
        self.replay_safe_tools.contains(name)
    }

    pub(crate) fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Clone the immutable host hook set so a resolved snapshot can wrap it
    /// with freshly compiled, source-pinned policy hooks.
    pub(crate) fn hook_set(&self) -> Arc<dyn HookSet> {
        Arc::clone(&self.hooks)
    }

    pub(crate) fn artifact_policy_config(&self) -> &ArtifactPolicy {
        &self.artifact_policy
    }

    /// Clone this executable capability set with an already validated,
    /// immutable provider-visible system prompt.
    ///
    /// Harness lineage resolves source and policy before calling this helper;
    /// the core template deliberately does not inspect session state or source
    /// trees.  A running epoch owns the returned clone, so later activation
    /// cannot mutate its configuration in place.
    pub(crate) fn with_resolved_system_prompt(&self, system_prompt: String) -> Self {
        let mut resolved = self.clone();
        resolved.system_prompt = system_prompt;
        resolved
    }

    /// Add source-validated plugin capabilities and hooks to an otherwise
    /// trusted core template. The caller provides only executable tools whose
    /// host bindings were checked against the selected immutable snapshot.
    /// Name collisions fail closed; replacing a trusted tool would make an
    /// active revision ambiguous.
    pub(crate) fn with_resolved_plugins(
        &self,
        system_prompt: String,
        hooks: Arc<dyn HookSet>,
        plugin_tools: ToolRegistry,
    ) -> Result<Self, HarnessError> {
        let mut resolved = self.with_resolved_system_prompt(system_prompt);
        for name in plugin_tools.names().map(str::to_owned).collect::<Vec<_>>() {
            if resolved.tools.get(&name).is_some() {
                return Err(HarnessError::invalid_state(format!(
                    "source-pinned plugin tool {name} collides with a trusted template capability",
                )));
            }
            let tool = plugin_tools
                .get(&name)
                .expect("resolved plugin tool remains registered")
                .clone();
            resolved.tools.insert(tool);
        }
        resolved.hooks = hooks;
        Ok(resolved)
    }

    /// Build one immutable core epoch after adding host-owned stable tools.
    ///
    /// The caller must reject name collisions before this point; replacing an
    /// editable tool would silently change the frozen snapshot contract.
    pub(crate) fn build_agent_with_tools(
        &self,
        effect_gate: Arc<dyn EffectGate>,
        provenance: RunProvenance,
        additional_tools: ToolRegistry,
    ) -> Result<Agent, HarnessError> {
        let mut tools = self.tools.clone();
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
            .system_prompt(self.system_prompt.clone())
            .tools(tools)
            .model_provider(Arc::clone(&self.provider))
            .hooks(Arc::clone(&self.hooks))
            .effect_gate(effect_gate)
            .effect_provenance(provenance)
            .thinking_level(self.thinking_level)
            .tool_failure_circuit_breaker(self.tool_failure_circuit_breaker);
        if let Some(model) = &self.model {
            builder = builder.model(model.clone());
        }
        if let Some(compactor) = &self.compactor {
            builder = builder.compactor(Arc::clone(compactor));
        }
        builder = builder
            .automatic_compaction(self.automatic_compaction.clone())?
            .tool_result_projection(self.tool_result_projection.clone())?;
        Ok(builder.build())
    }
}
