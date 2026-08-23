//! Durable ownership boundary for ABI-v1 policy lifecycle state.
//!
//! Lua evaluates bounded, capability-free callbacks here, but it never sees a
//! session writer or another plugin's state. The supervisor calls this module
//! immediately before committing operation and epoch records; recovery calls
//! it only to rebuild process-local VM state from those already committed
//! values.

use crate::harness::HarnessError;
use std::collections::BTreeMap;
use std::sync::Arc;
use tea_core::harness::extension::ExtensionLifecycle;
use tea_session::{JsonValue, StableHookId};

/// Freshly compiled lifecycle policies selected by one immutable harness
/// snapshot. The order is the snapshot's deterministic global-then-session
/// plugin registry order.
#[derive(Clone, Default)]
pub(crate) struct PluginLifecycleRegistry {
    policies: Vec<PluginLifecyclePolicy>,
}

#[derive(Clone)]
struct PluginLifecyclePolicy {
    plugin_id: String,
    policy: Arc<dyn ExtensionLifecycle>,
}

impl PluginLifecycleRegistry {
    /// Preserve only process-local extension lifecycles from source-pinned
    /// resolution; durable identities remain `StableHookId`s at the session
    /// boundary.
    pub(crate) fn from_resolved(
        policies: impl IntoIterator<Item = (String, Arc<dyn ExtensionLifecycle>)>,
    ) -> Result<Self, HarnessError> {
        let policies = policies
            .into_iter()
            .map(|(plugin_id, policy)| {
                for local_id in policy.hook_ids().map_err(extension_error)? {
                    let _ = stable_hook_id(&plugin_id, &local_id)?;
                }
                Ok(PluginLifecyclePolicy { plugin_id, policy })
            })
            .collect::<Result<Vec<_>, HarnessError>>()?;
        Ok(Self { policies })
    }

    /// Evaluate state proposals before an operation record exists. The caller
    /// must append the returned map in the same durable operation-start
    /// record before it starts any core effect.
    pub(crate) fn before_operation(
        &self,
    ) -> Result<BTreeMap<StableHookId, JsonValue>, HarnessError> {
        self.collect_state("before_operation", |policy| policy.before_operation())
    }

    /// Evaluate state proposals before an epoch record exists. The caller
    /// must append the returned map in the same durable epoch-start record
    /// before it starts a provider or tool effect.
    pub(crate) fn before_epoch(&self) -> Result<BTreeMap<StableHookId, JsonValue>, HarnessError> {
        self.collect_state("before_epoch", |policy| policy.before_epoch())
    }

    /// Rebuild process-local policy VM state from the exact persisted values
    /// owned by each stable registration. No result is written: this is
    /// intentionally repeatable after a crash before a subsequent durable
    /// consumer commits.
    pub(crate) fn before_resume(
        &self,
        operation_data: &BTreeMap<StableHookId, JsonValue>,
        epoch_data: &BTreeMap<StableHookId, JsonValue>,
    ) -> Result<(), HarnessError> {
        for registered in &self.policies {
            let mut operation = BTreeMap::new();
            let mut epoch = BTreeMap::new();
            for local_id in registered.policy.hook_ids().map_err(extension_error)? {
                let stable = stable_hook_id(&registered.plugin_id, &local_id)?;
                if let Some(value) = operation_data.get(&stable) {
                    operation.insert(local_id.clone(), value.clone());
                }
                if let Some(value) = epoch_data.get(&stable) {
                    epoch.insert(local_id, value.clone());
                }
            }
            registered
                .policy
                .before_resume(&operation, &epoch)
                .map_err(extension_error)?;
        }
        Ok(())
    }

    fn collect_state(
        &self,
        phase: &str,
        invoke: impl Fn(
            &dyn ExtensionLifecycle,
        ) -> Result<
            BTreeMap<String, JsonValue>,
            tea_core::harness::extension::ExtensionError,
        >,
    ) -> Result<BTreeMap<StableHookId, JsonValue>, HarnessError> {
        let mut durable = BTreeMap::new();
        for registered in &self.policies {
            let values = invoke(registered.policy.as_ref()).map_err(extension_error)?;
            for (local_id, value) in values {
                let stable = stable_hook_id(&registered.plugin_id, &local_id)?;
                if durable.insert(stable.clone(), value).is_some() {
                    return Err(HarnessError::invalid_state(format!(
                        "{phase} lifecycle state repeated stable hook ID {stable}",
                    )));
                }
            }
        }
        Ok(durable)
    }
}

fn stable_hook_id(plugin_id: &str, local_id: &str) -> Result<StableHookId, HarnessError> {
    StableHookId::new(format!("{plugin_id}.{local_id}")).map_err(|error| {
        HarnessError::invalid_state(format!(
            "plugin {plugin_id} lifecycle hook {local_id:?} cannot form a durable stable ID: {error}",
        ))
    })
}

fn extension_error(error: tea_core::harness::extension::ExtensionError) -> HarnessError {
    HarnessError::invalid_state(format!("extension lifecycle failed: {error}"))
}
