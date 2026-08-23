//! HookSet adapter for the policy-owned pre-tool decision.

use super::{LuaPolicy, PolicyError, PolicyMemoryProposal};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tea_core::error::HookError;
use tea_core::hooks::{
    AfterToolCall, AgentLoopTurnUpdate, BeforeToolCall, ContextEnvelope, HookFuture, HookSet,
    Replacement,
};
use tea_core::scheduler::CancellationToken;
use tea_core::tool::{AgentToolResult, ToolCall};

/// A hook adapter that gives a Lua policy the first, narrow pre-tool decision.
///
/// All other hook methods—including provider-context conversion—remain owned
/// by the embedding host. A denied call never reaches the wrapped hook set.
#[derive(Clone)]
pub struct LuaPolicyHookSet {
    policy: Arc<LuaPolicy>,
    inner: Arc<dyn HookSet>,
    memory: Option<PolicyMemoryBinding>,
}

/// One source-pinned plugin proposal emitted by a completed post-tool hook.
///
/// This is process-local hand-off data, not a session entry. The durable
/// harness consumes it only after raw tool evidence has been retained.
#[derive(Clone, Debug, PartialEq)]
pub struct CollectedPolicyMemoryProposal {
    /// Immutable plugin identity selected by the harness snapshot.
    pub plugin_id: String,
    /// Rust-validated policy proposal without a durable parent/entry pointer.
    pub proposal: PolicyMemoryProposal,
}

/// Per-epoch collector that bridges one completed core hook to the durable
/// supervisor's post-tool settlement boundary.
#[derive(Clone)]
pub struct PolicyMemoryCollector {
    pending: Arc<Mutex<BTreeMap<String, BTreeMap<usize, CollectedPolicyMemoryProposal>>>>,
}

impl Default for PolicyMemoryCollector {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl std::fmt::Debug for PolicyMemoryCollector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending = self
            .pending
            .lock()
            .map(|values| values.len())
            .unwrap_or_default();
        formatter
            .debug_struct("PolicyMemoryCollector")
            .field("pending_calls", &pending)
            .finish()
    }
}

impl PolicyMemoryCollector {
    /// Take the deterministic registry-ordered proposals for one completed
    /// tool call. A call is consumed at most once by the durable supervisor.
    pub fn take_for_call(
        &self,
        tool_call_id: &str,
    ) -> Result<Vec<CollectedPolicyMemoryProposal>, PolicyError> {
        let mut pending = self.pending.lock().map_err(|_| PolicyError::Runtime {
            message: "policy memory collector lock was poisoned".into(),
        })?;
        Ok(pending
            .remove(tool_call_id)
            .unwrap_or_default()
            .into_values()
            .collect())
    }

    fn record(
        &self,
        tool_call_id: &str,
        registry_index: usize,
        proposal: CollectedPolicyMemoryProposal,
    ) -> Result<(), PolicyError> {
        let mut pending = self.pending.lock().map_err(|_| PolicyError::Runtime {
            message: "policy memory collector lock was poisoned".into(),
        })?;
        let proposals = pending.entry(tool_call_id.into()).or_default();
        if proposals.insert(registry_index, proposal).is_some() {
            return Err(PolicyError::Runtime {
                message: format!(
                    "post-tool policy registration {registry_index} emitted more than one memory proposal for call {tool_call_id}"
                ),
            });
        }
        Ok(())
    }
}

#[derive(Clone)]
struct PolicyMemoryBinding {
    plugin_id: String,
    registry_index: usize,
    collector: Arc<PolicyMemoryCollector>,
}

impl LuaPolicyHookSet {
    /// Compose a loaded policy with the host's provider and lifecycle hooks.
    pub fn new(policy: Arc<LuaPolicy>, inner: Arc<dyn HookSet>) -> Self {
        Self {
            policy,
            inner,
            memory: None,
        }
    }

    /// Compose a policy that may propose typed memory with a source-pinned
    /// registry identity and an epoch-local collector. Only the durable host
    /// can consume the collector; policies never receive it or a session
    /// mutation capability.
    pub fn new_with_memory(
        policy: Arc<LuaPolicy>,
        plugin_id: impl Into<String>,
        registry_index: usize,
        collector: Arc<PolicyMemoryCollector>,
        inner: Arc<dyn HookSet>,
    ) -> Self {
        Self {
            policy,
            inner,
            memory: Some(PolicyMemoryBinding {
                plugin_id: plugin_id.into(),
                registry_index,
                collector,
            }),
        }
    }
}

impl HookSet for LuaPolicyHookSet {
    fn before_tool_call(&self, call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        match self
            .policy
            .before_tool_call(call)
            .map_err(before_hook_error)?
        {
            BeforeToolCall::Allow => self.inner.before_tool_call(call),
            decision => Ok(decision),
        }
    }

    fn after_tool_call(
        &self,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<AfterToolCall, HookError> {
        let output = self
            .policy
            .after_tool_output(call, result)
            .map_err(after_hook_error)?;
        let mut projected = result.clone();
        apply_projection(&mut projected, output.projection);
        let host_projection = self.inner.after_tool_call(call, &projected)?;
        apply_projection(&mut projected, host_projection);
        record_memory(&self.memory, call, output.memory).map_err(after_hook_error)?;
        Ok(projection_delta(result, &projected))
    }

    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        self.inner.transform_context(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        self.inner.convert_to_llm(context)
    }

    fn should_stop_after_turn(&self, context: &ContextEnvelope) -> Result<bool, HookError> {
        self.inner.should_stop_after_turn(context)
    }

    fn prepare_next_turn(
        &self,
        context: ContextEnvelope,
    ) -> Result<AgentLoopTurnUpdate, HookError> {
        self.inner.prepare_next_turn(context)
    }

    fn before_tool_call_async<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ContextEnvelope,
        cancellation: CancellationToken,
    ) -> HookFuture<'a, BeforeToolCall> {
        match self
            .policy
            .before_tool_call(call)
            .map_err(before_hook_error)
        {
            Ok(BeforeToolCall::Allow) => {
                self.inner
                    .before_tool_call_async(call, context, cancellation)
            }
            Ok(decision) => Box::pin(std::future::ready(Ok(decision))),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }

    fn after_tool_call_async<'a>(
        &'a self,
        call: &'a ToolCall,
        result: &'a AgentToolResult,
        context: ContextEnvelope,
        cancellation: CancellationToken,
    ) -> HookFuture<'a, AfterToolCall> {
        let policy_output = match self
            .policy
            .after_tool_output(call, result)
            .map_err(after_hook_error)
        {
            Ok(output) => output,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let mut projected = result.clone();
        apply_projection(&mut projected, policy_output.projection);
        let inner = Arc::clone(&self.inner);
        let memory = self.memory.clone();
        let proposal = policy_output.memory;
        Box::pin(async move {
            let host_projection = inner
                .after_tool_call_async(call, &projected, context, cancellation)
                .await?;
            apply_projection(&mut projected, host_projection);
            record_memory(&memory, call, proposal).map_err(after_hook_error)?;
            Ok(projection_delta(result, &projected))
        })
    }

    fn transform_context_async<'a>(
        &'a self,
        context: ContextEnvelope,
        cancellation: CancellationToken,
    ) -> HookFuture<'a, ContextEnvelope> {
        self.inner.transform_context_async(context, cancellation)
    }

    fn convert_to_llm_async<'a>(
        &'a self,
        context: ContextEnvelope,
        cancellation: CancellationToken,
    ) -> HookFuture<'a, String> {
        self.inner.convert_to_llm_async(context, cancellation)
    }

    fn should_stop_after_turn_async<'a>(
        &'a self,
        context: &'a ContextEnvelope,
        cancellation: CancellationToken,
    ) -> HookFuture<'a, bool> {
        self.inner
            .should_stop_after_turn_async(context, cancellation)
    }

    fn prepare_next_turn_async<'a>(
        &'a self,
        context: ContextEnvelope,
        cancellation: CancellationToken,
    ) -> HookFuture<'a, AgentLoopTurnUpdate> {
        self.inner.prepare_next_turn_async(context, cancellation)
    }
}

fn before_hook_error(error: PolicyError) -> HookError {
    HookError::new("before_tool_call", error.to_string())
}

fn after_hook_error(error: PolicyError) -> HookError {
    HookError::new("after_tool", error.to_string())
}

fn record_memory(
    binding: &Option<PolicyMemoryBinding>,
    call: &ToolCall,
    proposal: Option<PolicyMemoryProposal>,
) -> Result<(), PolicyError> {
    let (Some(binding), Some(proposal)) = (binding, proposal) else {
        return Ok(());
    };
    binding.collector.record(
        call.id.as_str(),
        binding.registry_index,
        CollectedPolicyMemoryProposal {
            plugin_id: binding.plugin_id.clone(),
            proposal,
        },
    )
}

/// Apply exactly the fields a core hook is allowed to replace. Keeping this
/// composition local makes a Luau policy an ordinary hook adapter: it has no
/// route to mutate a raw artifact or usage ledger.
fn apply_projection(result: &mut AgentToolResult, projection: AfterToolCall) {
    if let Replacement::Replace(content) = projection.content {
        result.content = content;
    }
    if let Replacement::Replace(details) = projection.details {
        result.details = details;
    }
    if let Replacement::Replace(is_error) = projection.is_error {
        result.is_error = is_error;
    }
    if let Replacement::Replace(failure) = projection.failure {
        result.failure = failure;
    }
    if let Replacement::Replace(usage) = projection.usage {
        result.usage = Some(usage);
    }
    if let Replacement::Replace(names) = projection.added_tool_names {
        result.added_tool_names = names;
    }
    if let Some(terminate) = projection.terminate {
        result.terminate = terminate;
    }
}

fn replacement<T: Eq>(original: T, projected: T) -> Replacement<T> {
    if original == projected {
        Replacement::Keep
    } else {
        Replacement::Replace(projected)
    }
}

fn projection_delta(original: &AgentToolResult, projected: &AgentToolResult) -> AfterToolCall {
    AfterToolCall {
        content: replacement(original.content.clone(), projected.content.clone()),
        details: replacement(original.details.clone(), projected.details.clone()),
        is_error: replacement(original.is_error, projected.is_error),
        failure: replacement(original.failure.clone(), projected.failure.clone()),
        // Core's replacement type cannot clear usage; neither can a policy.
        // A host may attach usage when it was absent, but a projection never
        // invents or removes the raw accounting value.
        usage: match (&original.usage, &projected.usage) {
            (_, Some(usage)) if original.usage.as_ref() != Some(usage) => {
                Replacement::Replace(usage.clone())
            }
            _ => Replacement::Keep,
        },
        added_tool_names: replacement(
            original.added_tool_names.clone(),
            projected.added_tool_names.clone(),
        ),
        terminate: (original.terminate != projected.terminate).then_some(projected.terminate),
    }
}
