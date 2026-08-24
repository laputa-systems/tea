//! Host-owned executable services for durable sessions.

use crate::agent::{Agent, AgentConfiguration};
use crate::harness::lineage::runtime_hook_bundle_digest;
use crate::harness::{HarnessError, ResolvedHarness};
use std::collections::BTreeSet;
use std::sync::Arc;
use tea_core::compaction::{AutomaticCompactionPolicy, Compactor};
use tea_core::effect::{EffectGate, RunProvenance};
use tea_core::hooks::{HookSet, NoHooks};
use tea_core::scheduler::ModelProvider;
use tea_core::state::{ModelDescriptor, ThinkingLevel};
use tea_core::tool::{ToolFailureCircuitBreaker, ToolRegistry, ToolResultProjectionPolicy};
use tea_session::{ArtifactPolicy, CanonicalHashWriter, Digest};

/// Stable identities for the executable runtime policies installed in one
/// [`RuntimeServices`] value.
///
/// These values are copied into an immutable harness snapshot at seed time.
/// Resolution checks them again before constructing an agent, so a snapshot
/// cannot silently run with a different hook or policy implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimePolicyIdentities {
    /// Identity of the installed hook bundle.
    pub hook_bundle_digest: Digest,
    /// Identity of the installed automatic-compaction policy.
    pub compaction_policy_digest: Digest,
    /// Identity of the installed tool-result projection policy.
    pub tool_projection_digest: Digest,
    /// Identity of the installed tool-failure policy.
    pub failure_policy_digest: Digest,
}

const DEFAULT_HOOK_BUNDLE_IDENTITY: &str = "tea-runtime-hooks-v1";

fn automatic_compaction_identity(policy: &AutomaticCompactionPolicy) -> Digest {
    let mut writer = CanonicalHashWriter::new("tea-runtime-automatic-compaction", 1, 1);
    writer.boolean("enabled", policy.enabled);
    writer.u64("context_budget_tokens", policy.context_budget.tokens());
    writer.string(
        "context_budget_kind",
        match policy.context_budget {
            crate::compaction::ContextBudgetSource::ContextWindow(_) => "context_window",
            crate::compaction::ContextBudgetSource::ContextBudget(_) => "context_budget",
        },
    );
    writer.u64("reserved_tokens", policy.reserved_tokens);
    writer.u64("minimum_headroom_tokens", policy.minimum_headroom_tokens);
    writer.u64("recent_tokens", policy.recent_tokens);
    writer.string(
        "overflow_recovery",
        match policy.overflow_recovery {
            crate::compaction::OverflowRecovery::Disabled => "disabled",
            crate::compaction::OverflowRecovery::CompactAndRetry => "compact_and_retry",
        },
    );
    writer.u64(
        "max_compactions_per_run",
        u64::from(policy.max_compactions_per_run),
    );
    writer.u64(
        "max_overflow_retries_per_run",
        u64::from(policy.max_overflow_retries_per_run),
    );
    writer.finish()
}

fn tool_projection_identity(policy: &ToolResultProjectionPolicy) -> Digest {
    let mut writer = CanonicalHashWriter::new("tea-runtime-tool-projection", 1, 1);
    writer.u64("max_content_bytes", policy.max_content_bytes as u64);
    writer.u64("max_details_bytes", policy.max_details_bytes as u64);
    writer.u64("max_total_bytes", policy.max_total_bytes as u64);
    writer.boolean(
        "deduplicate_repeated_errors",
        policy.deduplicate_repeated_errors,
    );
    writer.finish()
}

fn failure_policy_identity(policy: ToolFailureCircuitBreaker) -> Digest {
    let mut writer = CanonicalHashWriter::new("tea-runtime-failure-policy", 1, 1);
    writer.u64(
        "max_consecutive_retryable_failures",
        policy
            .max_consecutive_retryable_failures
            .map_or(0, |value| u64::from(value.get())),
    );
    writer.boolean(
        "has_max_consecutive_retryable_failures",
        policy.max_consecutive_retryable_failures.is_some(),
    );
    writer.finish()
}

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
    policy_identities: RuntimePolicyIdentities,
    replay_safe_tools: BTreeSet<String>,
    artifact_policy: ArtifactPolicy,
    prompt_layout_scope: crate::measurement::PromptCacheScope,
    prompt_layout_policy: crate::measurement::PromptLayoutPolicy,
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
            policy_identities: RuntimePolicyIdentities {
                hook_bundle_digest: Digest::from_bytes(DEFAULT_HOOK_BUNDLE_IDENTITY),
                compaction_policy_digest: automatic_compaction_identity(
                    &AutomaticCompactionPolicy::default(),
                ),
                tool_projection_digest: tool_projection_identity(
                    &ToolResultProjectionPolicy::default(),
                ),
                failure_policy_digest: failure_policy_identity(ToolFailureCircuitBreaker::default()),
            },
            replay_safe_tools: BTreeSet::new(),
            artifact_policy: ArtifactPolicy::default(),
            prompt_layout_scope: crate::measurement::PromptCacheScope::default(),
            prompt_layout_policy: crate::measurement::PromptLayoutPolicy::default(),
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

    /// Borrow the exact provider/model/revision descriptor selected for this
    /// lane, when the host bound one.
    pub(crate) fn model_descriptor(&self) -> Option<&ModelDescriptor> {
        self.model.as_ref()
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
        let identity = hooks.identity();
        self.base_hooks = hooks;
        self.policy_identities.hook_bundle_digest = identity;
        self
    }

    /// Install host hooks with an explicit immutable bundle identity.
    pub fn hooks_with_identity(mut self, hooks: Arc<dyn HookSet>, identity: Digest) -> Self {
        self.base_hooks = hooks;
        self.policy_identities.hook_bundle_digest = identity;
        self
    }

    /// Install a caller-owned compactor without giving the harness a provider implementation.
    pub fn compactor(mut self, compactor: Arc<dyn Compactor>) -> Self {
        self.compactor = Some(compactor);
        self
    }

    /// Set the explicit automatic-compaction policy.
    pub fn automatic_compaction(mut self, policy: AutomaticCompactionPolicy) -> Self {
        self.policy_identities.compaction_policy_digest = automatic_compaction_identity(&policy);
        self.automatic_compaction = policy;
        self
    }

    /// Set bounded model-facing tool-result projection policy.
    pub fn tool_result_projection(mut self, policy: ToolResultProjectionPolicy) -> Self {
        self.policy_identities.tool_projection_digest = tool_projection_identity(&policy);
        self.tool_result_projection = policy;
        self
    }

    /// Set the per-run repeated tool-failure circuit breaker policy.
    pub fn tool_failure_circuit_breaker(mut self, policy: ToolFailureCircuitBreaker) -> Self {
        self.policy_identities.failure_policy_digest = failure_policy_identity(policy);
        self.tool_failure_circuit_breaker = policy;
        self
    }

    /// Select an opaque equality-only serving/cache scope for the ledgers
    /// that lanes build from this immutable service configuration.
    pub fn prompt_cache_scope(mut self, scope: crate::measurement::PromptCacheScope) -> Self {
        self.prompt_layout_scope = scope;
        self
    }

    /// Select whether layout continuity is observed or rejected before
    /// provider dispatch.
    pub fn prompt_layout_policy(mut self, policy: crate::measurement::PromptLayoutPolicy) -> Self {
        self.prompt_layout_policy = policy;
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
        self.prompt_layout_scope
    }

    pub(crate) fn prompt_layout_policy_value(&self) -> crate::measurement::PromptLayoutPolicy {
        self.prompt_layout_policy
    }

    pub(crate) fn replay_safe_tools(&self) -> &BTreeSet<String> {
        &self.replay_safe_tools
    }

    pub(crate) fn artifact_policy_config(&self) -> &ArtifactPolicy {
        &self.artifact_policy
    }

    pub(crate) fn verify_runtime_policy_identities(
        &self,
        resolved: &ResolvedHarness,
    ) -> Result<(), HarnessError> {
        let Some(snapshot) = resolved.harness_snapshot.as_ref() else {
            return Ok(());
        };
        let identities = self.runtime_policy_identities();
        let checks = [
            (
                "hook bundle",
                runtime_hook_bundle_digest(identities.hook_bundle_digest, &snapshot.spec),
                snapshot.spec.hook_bundle_digest,
            ),
            (
                "automatic compaction",
                identities.compaction_policy_digest,
                snapshot.spec.compaction_policy_digest,
            ),
            (
                "tool-result projection",
                identities.tool_projection_digest,
                snapshot.spec.tool_projection_digest,
            ),
            (
                "tool-failure",
                identities.failure_policy_digest,
                snapshot.spec.failure_policy_digest,
            ),
        ];
        for (label, expected, actual) in checks {
            if expected != actual {
                return Err(HarnessError::invalid_state(format!(
                    "resolved harness {label} identity does not match RuntimeServices (expected {}, actual {})",
                    expected.to_hex(),
                    actual.to_hex(),
                )));
            }
        }
        Ok(())
    }

    /// Return the identities paired with this service set's executable policies.
    pub fn runtime_policy_identities(&self) -> RuntimePolicyIdentities {
        self.policy_identities
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
        prompt_layout_ledger: Arc<crate::measurement::PromptLayoutLedger>,
        additional_tools: ToolRegistry,
        host_messages: Vec<crate::state::SerializedJson>,
    ) -> Result<Agent, HarnessError> {
        self.verify_runtime_policy_identities(resolved)?;
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
        builder = builder.prompt_layout_ledger(prompt_layout_ledger);
        if let Some(model) = &self.model {
            builder = builder.model(model.clone());
        }
        if let Some(compactor) = &self.compactor {
            builder = builder.compactor(Arc::clone(compactor));
        }
        for message in host_messages {
            builder = builder.host_message(message);
        }
        builder = builder
            .automatic_compaction(resolved.automatic_compaction_policy().clone())?
            .tool_result_projection(resolved.tool_result_projection_policy().clone())?;
        Ok(builder.build())
    }
}
