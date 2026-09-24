//! Host-owned capability bindings for executable Luau plugin tools.
//!
//! A source bundle may request a capability name, but source is never the
//! grant. The session host constructs this catalog before the manager is
//! shared, and resolution creates a fresh snapshot-bound adapter for exactly
//! one plugin/capability pair. The adapter is intentionally not durable: only
//! its stable identity is persisted in a [`CapabilityBindingRef`](crate::CapabilityBindingRef).

use crate::harness::HarnessError;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use tea_core::harness::extension::{
    ExtensionCapability, ExtensionCapabilityBindings, ExtensionCapabilityError,
    ExtensionCapabilityFuture, ExtensionCapabilityRequest, ExtensionCapabilityResponse,
    ExtensionStateGeneration, ExtensionStateHandle, ExtensionStateUpdate, ExtensionToolLimits,
};
use tea_protocol::JsonValue;
use tea_session::{CanonicalHashWriter, Digest, HarnessSnapshotId};

const EXTENSION_STATE_CAPABILITY: &str = "extension.state";

/// One host-owned, versioned capability implementation that a particular
/// plugin may use after its immutable snapshot has been resolved.
#[derive(Clone)]
pub struct PluginCapabilityBinding {
    plugin_id: String,
    capability: String,
    capability_version: String,
    binding_digest: Digest,
    handler_limits: ExtensionToolLimits,
    implementation: Arc<dyn ExtensionCapability>,
    state_handle: Option<ExtensionStateHandle>,
}

impl fmt::Debug for PluginCapabilityBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginCapabilityBinding")
            .field("plugin_id", &self.plugin_id)
            .field("capability", &self.capability)
            .field("capability_version", &self.capability_version)
            .field("binding_digest", &self.binding_digest)
            .field("handler_limits", &self.handler_limits)
            .finish_non_exhaustive()
    }
}

impl PluginCapabilityBinding {
    /// Construct a host-owned grant.
    ///
    /// `host_identity` identifies the trusted host implementation/configuration
    /// without serializing its handle or secret. It is combined with the
    /// plugin ID, version, and resource limits into the durable binding digest
    /// named by a harness snapshot.
    pub fn new(
        plugin_id: impl Into<String>,
        capability: impl Into<String>,
        capability_version: impl Into<String>,
        host_identity: Digest,
        handler_limits: ExtensionToolLimits,
        implementation: Arc<dyn ExtensionCapability>,
    ) -> Result<Self, CapabilityBindingError> {
        let capability = capability.into();
        if capability == EXTENSION_STATE_CAPABILITY {
            return Err(CapabilityBindingError::ExtensionStateRequiresDedicatedBinding);
        }
        Self::new_inner(
            plugin_id.into(),
            capability,
            capability_version.into(),
            host_identity,
            handler_limits,
            implementation,
            None,
        )
    }

    /// Construct the only host binding allowed to grant `extension.state`.
    ///
    /// The catalog retains the late-bound handle separately from its normal
    /// capability implementation. Epoch resolution then mints a fresh,
    /// generation-bound capability rather than allowing a catalog-level
    /// implementation to select a lane or outlive its epoch.
    pub fn new_extension_state(
        plugin_id: impl Into<String>,
        capability_version: impl Into<String>,
        host_identity: Digest,
        handler_limits: ExtensionToolLimits,
        state_handle: ExtensionStateHandle,
    ) -> Result<Self, CapabilityBindingError> {
        let plugin_id = plugin_id.into();
        let implementation = Arc::new(ExtensionStateCapability::new(
            plugin_id.clone(),
            state_handle.clone(),
        )?);
        Self::new_inner(
            plugin_id,
            EXTENSION_STATE_CAPABILITY.into(),
            capability_version.into(),
            host_identity,
            handler_limits,
            implementation,
            Some(state_handle),
        )
    }

    fn new_inner(
        plugin_id: String,
        capability: String,
        capability_version: String,
        host_identity: Digest,
        handler_limits: ExtensionToolLimits,
        implementation: Arc<dyn ExtensionCapability>,
        state_handle: Option<ExtensionStateHandle>,
    ) -> Result<Self, CapabilityBindingError> {
        for (field, value) in [
            ("plugin_id", plugin_id.as_str()),
            ("capability", capability.as_str()),
            ("capability_version", capability_version.as_str()),
        ] {
            validate_portable_identifier(field, value)?;
        }
        validate_handler_limits(handler_limits)?;
        let binding_digest = binding_digest(
            &plugin_id,
            &capability,
            &capability_version,
            host_identity,
            handler_limits,
        );
        Ok(Self {
            plugin_id,
            capability,
            capability_version,
            binding_digest,
            handler_limits,
            implementation,
            state_handle,
        })
    }

    /// Stable plugin identity accepted by this grant.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Exact requested capability name accepted by this grant.
    pub fn capability(&self) -> &str {
        &self.capability
    }

    /// Host-selected capability ABI/version label.
    pub fn capability_version(&self) -> &str {
        &self.capability_version
    }

    /// Durable identity persisted in the immutable harness snapshot.
    pub fn binding_digest(&self) -> Digest {
        self.binding_digest
    }

    /// Exact handler limits selected by the host grant.
    pub const fn handler_limits(&self) -> ExtensionToolLimits {
        self.handler_limits
    }
}

/// Host catalog of explicit plugin capability grants.
#[derive(Clone, Default)]
pub struct PluginCapabilityCatalog {
    bindings: BTreeMap<(String, String), PluginCapabilityBinding>,
    fixed_tool_capabilities: BTreeMap<String, BTreeMap<String, String>>,
    additional_read_only_capabilities: BTreeMap<String, BTreeSet<String>>,
}

impl fmt::Debug for PluginCapabilityCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginCapabilityCatalog")
            .field("bindings", &self.bindings.keys().collect::<Vec<_>>())
            .field("fixed_tool_capabilities", &self.fixed_tool_capabilities)
            .field(
                "additional_read_only_capabilities",
                &self.additional_read_only_capabilities,
            )
            .finish()
    }
}

impl PluginCapabilityCatalog {
    /// Create an empty catalog. An empty catalog is valid for a session whose
    /// immutable plugin registry requests no capabilities.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a single explicit grant. Duplicate plugin/capability pairs are
    /// rejected rather than silently replacing a host authority object.
    pub fn insert(
        &mut self,
        binding: PluginCapabilityBinding,
    ) -> Result<(), CapabilityBindingError> {
        let key = (binding.plugin_id.clone(), binding.capability.clone());
        if self.bindings.contains_key(&key) {
            return Err(CapabilityBindingError::Duplicate {
                plugin_id: key.0,
                capability: key.1,
            });
        }
        self.bindings.insert(key, binding);
        Ok(())
    }

    /// Freeze named model-tool authorities in one host-selected extension,
    /// plus the only capabilities future additive tools may use. This is
    /// separate from source because a revision must never turn (for example)
    /// a read tool into a process tool.
    pub fn fix_tool_capabilities(
        &mut self,
        plugin_id: impl Into<String>,
        tool_capabilities: BTreeMap<String, String>,
        additional_read_only_capabilities: BTreeSet<String>,
    ) -> Result<(), CapabilityBindingError> {
        let plugin_id = plugin_id.into();
        validate_portable_identifier("plugin_id", &plugin_id)?;
        for (tool, capability) in &tool_capabilities {
            validate_portable_identifier("tool", tool)?;
            validate_portable_identifier("capability", capability)?;
        }
        for capability in &additional_read_only_capabilities {
            validate_portable_identifier("capability", capability)?;
        }
        if self.fixed_tool_capabilities.contains_key(&plugin_id) {
            return Err(CapabilityBindingError::ToolGrantsAlreadyFixed { plugin_id });
        }
        self.fixed_tool_capabilities
            .insert(plugin_id.clone(), tool_capabilities);
        self.additional_read_only_capabilities
            .insert(plugin_id, additional_read_only_capabilities);
        Ok(())
    }

    pub(crate) fn fixed_tool_capabilities(
        &self,
        plugin_id: &str,
    ) -> Option<(&BTreeMap<String, String>, &BTreeSet<String>)> {
        self.fixed_tool_capabilities.get(plugin_id).map(|fixed| {
            (
                fixed,
                self.additional_read_only_capabilities
                    .get(plugin_id)
                    .expect("fixed tool grants retain their read-only capability set"),
            )
        })
    }

    /// Resolve a persisted reference into a capability set for one immutable
    /// snapshot. This is crate-private because only harness resolution may
    /// bind a script to an epoch configuration.
    pub(crate) fn bind(
        &self,
        plugin_id: &str,
        capability: &str,
        capability_version: &str,
        binding_digest: Digest,
        snapshot_id: &HarnessSnapshotId,
        resource_limits: &crate::harness::HarnessResourceLimits,
        state_generation: Option<&ExtensionStateGeneration>,
    ) -> Result<ResolvedCapabilityBinding, HarnessError> {
        let binding = self
            .bindings
            .get(&(plugin_id.to_owned(), capability.to_owned()))
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "plugin {plugin_id} capability {capability} has no trusted host binding",
                ))
            })?;
        if binding.capability_version != capability_version {
            return Err(HarnessError::invalid_state(format!(
                "plugin {plugin_id} capability {capability} is pinned to version {capability_version}, but the trusted host catalog provides {}",
                binding.capability_version,
            )));
        }
        if binding.binding_digest != binding_digest {
            return Err(HarnessError::invalid_state(format!(
                "plugin {plugin_id} capability {capability} does not match its immutable host-binding identity",
            )));
        }
        if binding.handler_limits.max_source_bytes > resource_limits.source_bytes
            || binding.handler_limits.max_memory_bytes > resource_limits.memory_bytes
            || binding.handler_limits.max_interrupt_checks
                > resource_limits.instruction_checks as usize
        {
            return Err(HarnessError::invalid_state(format!(
                "plugin {plugin_id} capability {capability} host handler limits exceed the immutable snapshot resource limits",
            )));
        }
        let implementation: Arc<dyn ExtensionCapability> = match &binding.state_handle {
            Some(state_handle) => {
                let state = state_generation
                    .map(|generation| state_handle.for_generation(generation.clone()))
                    .unwrap_or_else(|| state_handle.clone());
                Arc::new(
                    ExtensionStateCapability::new(binding.plugin_id.clone(), state)
                        .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
                )
            }
            None => Arc::clone(&binding.implementation),
        };
        let mut capabilities = ExtensionCapabilityBindings::new();
        capabilities
            .insert(
                capability.to_owned(),
                Arc::new(SnapshotBoundCapability {
                    plugin_id: plugin_id.to_owned(),
                    capability: capability.to_owned(),
                    snapshot_id: snapshot_id.clone(),
                    inner: implementation,
                }),
                binding.handler_limits,
            )
            .map_err(binding_error)?;
        Ok(ResolvedCapabilityBinding { capabilities })
    }
}

/// A capability set and exact limits resolved for one tool handler.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedCapabilityBinding {
    pub(crate) capabilities: ExtensionCapabilityBindings,
}

/// Failure while forming a trusted plugin-capability catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityBindingError {
    /// A stable identity component did not use the portable persisted spelling.
    InvalidIdentifier {
        /// Contract field whose spelling was invalid.
        field: &'static str,
        /// Rejected value.
        value: String,
    },
    /// A resource ceiling was zero.
    InvalidLimit {
        /// Contract field name.
        field: &'static str,
    },
    /// Two host objects tried to grant the same plugin/capability pair.
    Duplicate {
        /// Plugin identity.
        plugin_id: String,
        /// Capability name.
        capability: String,
    },
    /// The host attempted to replace a plugin's fixed tool-authority policy.
    ToolGrantsAlreadyFixed {
        /// Plugin identity.
        plugin_id: String,
    },
    /// `extension.state` must retain an epoch-bound state handle.
    ExtensionStateRequiresDedicatedBinding,
}

impl fmt::Display for CapabilityBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier { field, value } => write!(
                formatter,
                "plugin capability binding {field} must use the portable [A-Za-z0-9._-] spelling; got {value:?}",
            ),
            Self::InvalidLimit { field } => {
                write!(
                    formatter,
                    "plugin capability binding limit {field} must be greater than zero"
                )
            }
            Self::Duplicate {
                plugin_id,
                capability,
            } => write!(
                formatter,
                "plugin {plugin_id} capability {capability} is already bound by this host catalog",
            ),
            Self::ToolGrantsAlreadyFixed { plugin_id } => write!(
                formatter,
                "plugin {plugin_id} already has a fixed tool capability map in this host catalog",
            ),
            Self::ExtensionStateRequiresDedicatedBinding => write!(
                formatter,
                "extension.state must use PluginCapabilityBinding::new_extension_state",
            ),
        }
    }
}

impl std::error::Error for CapabilityBindingError {}

/// Generic capability exposing only one extension's whole-value state
/// namespace. The namespace is fixed by the trusted host at construction;
/// Luau may request only `get` and `replace` under the capability it was
/// explicitly granted.
#[derive(Clone, Debug)]
pub struct ExtensionStateCapability {
    extension_id: String,
    state: ExtensionStateHandle,
}

impl ExtensionStateCapability {
    /// Construct a state capability fixed to one immutable extension ID.
    pub fn new(
        extension_id: impl Into<String>,
        state: ExtensionStateHandle,
    ) -> Result<Self, CapabilityBindingError> {
        let extension_id = extension_id.into();
        validate_portable_identifier("plugin_id", &extension_id)?;
        Ok(Self {
            extension_id,
            state,
        })
    }
}

impl ExtensionCapability for ExtensionStateCapability {
    fn invoke(
        &self,
        request: ExtensionCapabilityRequest,
        cancellation: tea_core::scheduler::CancellationToken,
    ) -> ExtensionCapabilityFuture {
        let extension_id = self.extension_id.clone();
        let state = self.state.clone();
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(ExtensionCapabilityError::Cancelled);
            }

            match request.method.as_str() {
                "get" => state
                    .read(&extension_id)
                    .map_err(|error| ExtensionCapabilityError::Execution {
                        message: error.to_string(),
                    })
                    .map(|view| ExtensionCapabilityResponse {
                        value: view.value.unwrap_or(JsonValue::Null),
                    }),
                "replace" => {
                    let object = request.arguments.as_object().ok_or_else(|| {
                        ExtensionCapabilityError::InvalidArguments {
                            message: "extension.state replace arguments must be an object".into(),
                        }
                    })?;
                    let value = object.get("value").cloned().ok_or_else(|| {
                        ExtensionCapabilityError::InvalidArguments {
                            message: "extension.state replace requires value".into(),
                        }
                    })?;
                    state
                        .replace(&extension_id, ExtensionStateUpdate { value })
                        .map_err(|error| ExtensionCapabilityError::Execution {
                            message: error.to_string(),
                        })
                        .map(|()| ExtensionCapabilityResponse {
                            value: JsonValue::Bool(true),
                        })
                }
                method => Err(ExtensionCapabilityError::MethodDenied {
                    capability: request.capability,
                    method: method.to_owned(),
                }),
            }
        })
    }
}

struct SnapshotBoundCapability {
    plugin_id: String,
    capability: String,
    snapshot_id: HarnessSnapshotId,
    inner: Arc<dyn ExtensionCapability>,
}

impl ExtensionCapability for SnapshotBoundCapability {
    fn invoke(
        &self,
        request: ExtensionCapabilityRequest,
        cancellation: tea_core::scheduler::CancellationToken,
    ) -> ExtensionCapabilityFuture {
        if request.capability != self.capability {
            return Box::pin(std::future::ready(Err(
                ExtensionCapabilityError::NotBound {
                    capability: request.capability,
                },
            )));
        }
        // The wrapper is constructed only while resolving a particular
        // snapshot, and carries that identity for diagnostics/debuggers. It
        // deliberately delegates no ambient lookup: this exact host object is
        // the authority boundary for this handler invocation.
        let _immutable_binding = (&self.plugin_id, &self.snapshot_id);
        self.inner.invoke(request, cancellation)
    }
}

fn binding_digest(
    plugin_id: &str,
    capability: &str,
    capability_version: &str,
    host_identity: Digest,
    limits: ExtensionToolLimits,
) -> Digest {
    let mut writer = CanonicalHashWriter::new("tea-plugin-capability-binding-v1", 1, 1);
    writer.string("plugin_id", plugin_id);
    writer.string("capability", capability);
    writer.string("capability_version", capability_version);
    writer.bytes("host_identity", host_identity.as_bytes());
    writer.u64("max_source_bytes", limits.max_source_bytes as u64);
    writer.u64("max_memory_bytes", limits.max_memory_bytes as u64);
    writer.u64("max_interrupt_checks", limits.max_interrupt_checks as u64);
    writer.u64("max_capability_calls", limits.max_capability_calls as u64);
    writer.finish()
}

fn validate_portable_identifier(
    field: &'static str,
    value: &str,
) -> Result<(), CapabilityBindingError> {
    if value.is_empty()
        || value.len() > 120
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    {
        return Err(CapabilityBindingError::InvalidIdentifier {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_handler_limits(limits: ExtensionToolLimits) -> Result<(), CapabilityBindingError> {
    for (field, value) in [
        ("max_source_bytes", limits.max_source_bytes),
        ("max_memory_bytes", limits.max_memory_bytes),
        ("max_interrupt_checks", limits.max_interrupt_checks),
        ("max_capability_calls", limits.max_capability_calls),
    ] {
        if value == 0 {
            return Err(CapabilityBindingError::InvalidLimit { field });
        }
    }
    Ok(())
}

fn binding_error(error: tea_core::harness::extension::ExtensionError) -> HarnessError {
    HarnessError::invalid_state(format!("could not bind extension capability: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::extension::{
        ExtensionStateGeneration, ExtensionStateStore, ExtensionStateView,
    };
    use crate::scheduler::CancellationToken;
    use crate::state::ToolCallId;
    use crate::tool::ToolUpdateSink;
    use std::sync::Mutex;
    use tea_session::{EpochId, HarnessRevisionId, LaneId, OperationId};

    #[derive(Default)]
    struct MemoryStateStore {
        values: Mutex<BTreeMap<String, JsonValue>>,
    }

    impl ExtensionStateStore for MemoryStateStore {
        fn read_extension_state(
            &self,
            _generation: &ExtensionStateGeneration,
            extension_id: &str,
        ) -> Result<ExtensionStateView, crate::harness::extension::ExtensionError> {
            let value = self
                .values
                .lock()
                .expect("fixture state store lock")
                .get(extension_id)
                .cloned();
            Ok(ExtensionStateView { value })
        }

        fn replace_extension_state(
            &self,
            _generation: &ExtensionStateGeneration,
            extension_id: &str,
            update: ExtensionStateUpdate,
        ) -> Result<(), crate::harness::extension::ExtensionError> {
            self.values
                .lock()
                .expect("fixture state store lock")
                .insert(extension_id.to_owned(), update.value);
            Ok(())
        }
    }

    fn request(method: &str, arguments: JsonValue) -> ExtensionCapabilityRequest {
        ExtensionCapabilityRequest {
            call_id: ToolCallId::new("extension-state-capability-test")
                .expect("fixture tool call ID"),
            tool_name: "state_tool".into(),
            provenance: crate::effect::RunProvenance::default(),
            capability: "extension.state".into(),
            method: method.into(),
            arguments,
            updates: ToolUpdateSink::disabled(),
        }
    }

    fn state_generation() -> ExtensionStateGeneration {
        ExtensionStateGeneration::new(
            LaneId::main(),
            OperationId::new("extension-state-capability-operation").expect("fixture operation ID"),
            EpochId::new("extension-state-capability-epoch").expect("fixture epoch ID"),
            HarnessRevisionId::new("extension-state-capability-revision")
                .expect("fixture revision ID"),
        )
    }

    #[test]
    fn extension_state_capability_is_fixed_to_its_extension_namespace() {
        let handle = ExtensionStateHandle::new();
        let store = Arc::new(MemoryStateStore::default());
        handle
            .attach(Arc::clone(&store) as Arc<dyn ExtensionStateStore>)
            .expect("state store attaches once");
        let review =
            ExtensionStateCapability::new("review", handle.for_generation(state_generation()))
                .expect("portable extension ID");
        let other =
            ExtensionStateCapability::new("other", handle.for_generation(state_generation()))
                .expect("portable extension ID");

        let replaced = smol::block_on(review.invoke(
            request(
                "replace",
                JsonValue::parse(r#"{"value":{"phase":"open"}}"#).expect("fixture state JSON"),
            ),
            CancellationToken::new(),
        ))
        .expect("review can replace its state");
        assert_eq!(replaced.value, JsonValue::Bool(true));

        let review_state = smol::block_on(review.invoke(
            request("get", JsonValue::Object(BTreeMap::new())),
            CancellationToken::new(),
        ))
        .expect("review can read its state");
        assert_eq!(
            review_state.value,
            JsonValue::parse(r#"{"phase":"open"}"#).expect("fixture state JSON")
        );

        let other_state = smol::block_on(other.invoke(
            request("get", JsonValue::Object(BTreeMap::new())),
            CancellationToken::new(),
        ))
        .expect("other namespace remains readable");
        assert_eq!(other_state.value, JsonValue::Null);
    }

    #[test]
    fn extension_state_capability_rejects_an_unbound_catalog_handle() {
        let handle = ExtensionStateHandle::new();
        let store = Arc::new(MemoryStateStore::default());
        handle
            .attach(Arc::clone(&store) as Arc<dyn ExtensionStateStore>)
            .expect("state store attaches once");
        let capability =
            ExtensionStateCapability::new("review", handle).expect("portable extension ID");

        let error = smol::block_on(capability.invoke(
            request("get", JsonValue::Object(BTreeMap::new())),
            CancellationToken::new(),
        ))
        .expect_err("catalog-level state handles must not access a lane");

        assert!(error.to_string().contains("no active epoch generation"));
    }
}
