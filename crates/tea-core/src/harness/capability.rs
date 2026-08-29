//! Host-owned capability bindings for executable Luau plugin tools.
//!
//! A source bundle may request a capability name, but source is never the
//! grant. The session host constructs this catalog before the manager is
//! shared, and resolution creates a fresh snapshot-bound adapter for exactly
//! one plugin/capability pair. The adapter is intentionally not durable: only
//! its stable identity is persisted in a [`CapabilityBindingRef`](crate::CapabilityBindingRef).

use crate::harness::HarnessError;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tea_core::harness::extension::{
    ExtensionCapability, ExtensionCapabilityBindings, ExtensionCapabilityError,
    ExtensionCapabilityFuture, ExtensionCapabilityRequest, ExtensionCapabilityResponse,
    ExtensionStateHandle, ExtensionStateUpdate, ExtensionToolLimits,
};
use tea_protocol::JsonValue;
use tea_session::{CanonicalHashWriter, Digest, HarnessSnapshotId};

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
        let plugin_id = plugin_id.into();
        let capability = capability.into();
        let capability_version = capability_version.into();
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
}

impl fmt::Debug for PluginCapabilityCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginCapabilityCatalog")
            .field("bindings", &self.bindings.keys().collect::<Vec<_>>())
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
        let mut capabilities = ExtensionCapabilityBindings::new();
        capabilities
            .insert(
                capability.to_owned(),
                Arc::new(SnapshotBoundCapability {
                    plugin_id: plugin_id.to_owned(),
                    capability: capability.to_owned(),
                    snapshot_id: snapshot_id.clone(),
                    inner: Arc::clone(&binding.implementation),
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
        }
    }
}

impl std::error::Error for CapabilityBindingError {}

/// Generic capability exposing only one extension's append-only state
/// namespace. The namespace is fixed by the trusted host at construction;
/// Luau may request only `get` and `append` under the capability it was
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
                        value: JsonValue::Object(view.latest),
                    }),
                "append" => {
                    let object = request.arguments.as_object().ok_or_else(|| {
                        ExtensionCapabilityError::InvalidArguments {
                            message: "extension.state append arguments must be an object".into(),
                        }
                    })?;
                    let kind = object
                        .get("kind")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| ExtensionCapabilityError::InvalidArguments {
                            message: "extension.state append requires string kind".into(),
                        })?;
                    let content = object.get("content").cloned().ok_or_else(|| {
                        ExtensionCapabilityError::InvalidArguments {
                            message: "extension.state append requires content".into(),
                        }
                    })?;
                    state
                        .append(
                            &extension_id,
                            ExtensionStateUpdate {
                                kind: kind.to_owned(),
                                content,
                            },
                        )
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
    use crate::harness::extension::{ExtensionStateStore, ExtensionStateView};
    use crate::scheduler::CancellationToken;
    use crate::state::ToolCallId;
    use crate::tool::ToolUpdateSink;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryStateStore {
        values: Mutex<BTreeMap<(String, String), JsonValue>>,
    }

    impl ExtensionStateStore for MemoryStateStore {
        fn read_extension_state(
            &self,
            extension_id: &str,
        ) -> Result<ExtensionStateView, crate::harness::extension::ExtensionError> {
            let latest = self
                .values
                .lock()
                .expect("fixture state store lock")
                .iter()
                .filter(|((owner, _), _)| owner == extension_id)
                .map(|((_, kind), content)| (kind.clone(), content.clone()))
                .collect();
            Ok(ExtensionStateView { latest })
        }

        fn append_extension_state(
            &self,
            extension_id: &str,
            update: ExtensionStateUpdate,
        ) -> Result<(), crate::harness::extension::ExtensionError> {
            self.values
                .lock()
                .expect("fixture state store lock")
                .insert((extension_id.to_owned(), update.kind), update.content);
            Ok(())
        }
    }

    fn request(method: &str, arguments: JsonValue) -> ExtensionCapabilityRequest {
        ExtensionCapabilityRequest {
            call_id: ToolCallId::new("extension-state-capability-test")
                .expect("fixture tool call ID"),
            tool_name: "state_tool".into(),
            capability: "extension.state".into(),
            method: method.into(),
            arguments,
            updates: ToolUpdateSink::disabled(),
        }
    }

    #[test]
    fn extension_state_capability_is_fixed_to_its_extension_namespace() {
        let handle = ExtensionStateHandle::new();
        let store = Arc::new(MemoryStateStore::default());
        handle
            .attach(Arc::clone(&store) as Arc<dyn ExtensionStateStore>)
            .expect("state store attaches once");
        let review =
            ExtensionStateCapability::new("review", handle.clone()).expect("portable extension ID");
        let other = ExtensionStateCapability::new("other", handle).expect("portable extension ID");

        let appended = smol::block_on(
            review.invoke(
                request(
                    "append",
                    JsonValue::parse(r#"{"kind":"review.state.v1","content":{"phase":"open"}}"#)
                        .expect("fixture state JSON"),
                ),
                CancellationToken::new(),
            ),
        )
        .expect("review can append its state");
        assert_eq!(appended.value, JsonValue::Bool(true));

        let review_state = smol::block_on(review.invoke(
            request("get", JsonValue::Object(BTreeMap::new())),
            CancellationToken::new(),
        ))
        .expect("review can read its state");
        assert!(review_state.value.get("review.state.v1").is_some());

        let other_state = smol::block_on(other.invoke(
            request("get", JsonValue::Object(BTreeMap::new())),
            CancellationToken::new(),
        ))
        .expect("other namespace remains readable");
        assert_eq!(other_state.value, JsonValue::Object(BTreeMap::new()));
    }
}
