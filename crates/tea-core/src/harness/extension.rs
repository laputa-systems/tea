//! Narrow, language-neutral contracts for immutable harness extensions.
//!
//! The durable core owns the source identity, capability grants, and resulting core tools. An
//! adapter such as `tea-luau` owns parsing, VM construction, and language diagnostics.

use crate::error::HookError;
use crate::hooks::HookSet;
use crate::scheduler::CancellationToken;
use crate::state::ToolCallId;
use crate::tool::{
    CancellationSettlementMode, ToolExecutionMode, ToolRegistry, ToolUpdateSink,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use tea_protocol::JsonValue;

/// Exact closed source files selected for one immutable extension resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionSourceTree {
    /// Stable extension identity from the immutable harness registry.
    pub extension_id: String,
    /// UTF-8 source files keyed by their tree-relative portable path.
    pub files: BTreeMap<String, String>,
    /// Capabilities expected by the immutable extension reference.
    ///
    /// `None` is used only while staging a new source tree, before the
    /// extension engine has derived the source-declared set. Resolved
    /// snapshots always carry `Some` and engines must reject disagreement.
    pub expected_capabilities: Option<BTreeSet<String>>,
    /// Frozen resource ceilings for this source resolution.
    pub limits: ExtensionLimits,
}

/// Resource limits enforced by an extension implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtensionLimits {
    /// Maximum aggregate source bytes.
    pub max_source_bytes: usize,
    /// Maximum language-runtime memory bytes.
    pub max_memory_bytes: usize,
    /// Maximum cooperative interruption checks per evaluation.
    pub max_interrupt_checks: usize,
}

/// Model-visible prompt contribution declared by one extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionPromptSection {
    /// Extension-local stable section ID.
    pub id: String,
    /// Exact model-visible text.
    pub content: String,
}

/// Model-visible tool declaration produced by an extension.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionToolDescription {
    /// Stable model-visible tool name.
    pub name: String,
    /// Model-facing tool explanation.
    pub description: String,
    /// Canonical JSON Schema for tool arguments.
    pub schema: JsonValue,
    /// Capability that must be explicitly granted by the host.
    pub capability: String,
    /// Whether the core may overlap calls to this tool.
    pub execution_mode: ToolExecutionMode,
    /// Whether this tool must be the sole call in an assistant batch.
    pub requires_exclusive_batch: bool,
    /// How a started invocation settles after run cancellation.
    pub cancellation_settlement_mode: CancellationSettlementMode,
}

/// A terminal-host command contributed by an immutable extension.
///
/// This is deliberately separate from [`ExtensionToolDescription`]: commands
/// are a host-local control surface and are never advertised to a model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionHostCommandDescription {
    /// Literal slash-prefixed command name.
    pub name: String,
    /// Concise text for host completion and help surfaces.
    pub help: String,
    /// Whether the terminal host may accept the command while a model
    /// operation is settling. The host queues such controls and never starts
    /// a continuation until it has observed an idle durable lane.
    pub allowed_while_active: bool,
}

/// Latest inline values owned by one extension, indexed by its local kind.
///
/// The host derives this view from `PluginMemory`; an extension never receives
/// a session writer, entry IDs, another extension's values, or artifact paths.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtensionStateView {
    /// Latest value for each extension-local kind on the active branch.
    pub latest: BTreeMap<String, JsonValue>,
}

/// One append-only extension-local state update.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionStateUpdate {
    /// Versioned extension-local state kind.
    pub kind: String,
    /// Inline structured state. The durable runtime applies fixed
    /// external-only/session retention semantics for this control surface.
    pub content: JsonValue,
}

/// Bounded input supplied to an extension host-command handler.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionCommandInput {
    /// Text following the resolved slash command, without the command name.
    pub arguments: String,
    /// Latest durable state owned by the command's extension.
    pub state: ExtensionStateView,
}

/// A constrained result from an extension host-command handler.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtensionCommandResult {
    /// One bounded host notice or result line.
    pub notice: Option<String>,
    /// At most one append-only extension-local state update.
    pub state: Option<ExtensionStateUpdate>,
    /// Optional internal model context for a new durable operation. This is
    /// never represented as a user-authored chat message.
    pub internal_input: Option<String>,
}

/// Constrained executable host-command port for one resolved extension.
pub trait ExtensionHostCommand: Send + Sync {
    /// Immutable host-local command metadata.
    fn description(&self) -> &ExtensionHostCommandDescription;
    /// Evaluate the command without ambient host authority.
    fn invoke(&self, input: &ExtensionCommandInput) -> Result<ExtensionCommandResult, ExtensionError>;
}

/// Generic operation outcome made available to an idle extension hook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionOperationOutcome {
    /// The operation settled normally.
    Completed,
    /// A durable cancellation settled the operation.
    Aborted,
    /// A host-safe failure classification settled the operation.
    Failed { code: String },
}

/// Bounded metadata supplied after a durable operation has settled.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionIdleInput {
    /// Stable operation identity.
    pub operation_id: String,
    /// Terminal durable outcome.
    pub outcome: ExtensionOperationOutcome,
    /// Provider usage observed for this operation. Unknown fields remain
    /// absent instead of becoming zero.
    pub usage: tea_session::Usage,
    /// Wall-clock duration between the durable operation start and finish.
    pub elapsed_active_seconds: u64,
    /// Latest durable state owned by this extension.
    pub state: ExtensionStateView,
}

/// Result of an extension's optional idle hook.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtensionIdleResult {
    /// At most one append-only state update made before a continuation starts.
    pub state: Option<ExtensionStateUpdate>,
    /// At most one internal follow-up operation request.
    pub internal_input: Option<String>,
}

/// Executable idle policy for one resolved extension.
pub trait ExtensionIdleHook: Send + Sync {
    /// Evaluate only after the owning durable operation is terminal and the
    /// lane is idle. The host re-checks that condition before starting any
    /// returned continuation.
    fn on_idle(&self, input: &ExtensionIdleInput) -> Result<ExtensionIdleResult, ExtensionError>;
}

/// Narrow host port for one extension's durable state namespace.
///
/// Implementations are trusted host objects. The extension-facing capability
/// below fixes `extension_id` before this port is invoked, so Luau source
/// cannot select another extension's namespace or obtain a session writer.
pub trait ExtensionStateStore: Send + Sync {
    /// Read the latest inline value for every kind owned by one extension.
    fn read_extension_state(&self, extension_id: &str) -> Result<ExtensionStateView, ExtensionError>;
    /// Append one external-only/session-retained value to that extension's
    /// namespace. Existing values are never mutated.
    fn append_extension_state(
        &self,
        extension_id: &str,
        update: ExtensionStateUpdate,
    ) -> Result<(), ExtensionError>;
}

/// A late-bound host connection for a generic extension-state capability.
///
/// A capability catalog is constructed before a `SessionSupervisor` exists, so
/// the composition root attaches the already shared runtime exactly once.
/// This indirection carries no raw writer into Luau and is equally usable by
/// any bundled or user-supplied extension.
#[derive(Clone, Default)]
pub struct ExtensionStateHandle {
    store: Arc<Mutex<Option<Weak<dyn ExtensionStateStore>>>>,
}

impl fmt::Debug for ExtensionStateHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let attached = self.store.lock().map(|store| store.is_some()).unwrap_or(false);
        formatter
            .debug_struct("ExtensionStateHandle")
            .field("attached", &attached)
            .finish()
    }
}

impl ExtensionStateHandle {
    /// Create a detached handle for a host capability catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the one trusted store used by this live host harness.
    pub fn attach(&self, store: Arc<dyn ExtensionStateStore>) -> Result<(), ExtensionError> {
        let mut slot = self
            .store
            .lock()
            .map_err(|_| ExtensionError::new("extension state handle lock was poisoned"))?;
        if slot.is_some() {
            return Err(ExtensionError::new(
                "extension state handle is already attached",
            ));
        }
        *slot = Some(Arc::downgrade(&store));
        Ok(())
    }

    /// Read state through the attached trusted runtime.
    pub fn read(&self, extension_id: &str) -> Result<ExtensionStateView, ExtensionError> {
        let store = self
            .store
            .lock()
            .map_err(|_| ExtensionError::new("extension state handle lock was poisoned"))?
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| ExtensionError::new("extension state handle is not attached"))?;
        store.read_extension_state(extension_id)
    }

    /// Append state through the attached trusted runtime.
    pub fn append(
        &self,
        extension_id: &str,
        update: ExtensionStateUpdate,
    ) -> Result<(), ExtensionError> {
        let store = self
            .store
            .lock()
            .map_err(|_| ExtensionError::new("extension state handle lock was poisoned"))?
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| ExtensionError::new("extension state handle is not attached"))?;
        store.append_extension_state(extension_id, update)
    }
}

/// Provider-visible and lifecycle metadata validated from immutable source.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionDescriptor {
    /// Explicit capabilities declared by the closed source bundle.
    pub requested_capabilities: BTreeSet<String>,
    /// Ordered prompt contributions.
    pub prompt_sections: Vec<ExtensionPromptSection>,
    /// Ordered tool declarations.
    pub tools: Vec<ExtensionToolDescription>,
    /// Host-local slash commands contributed by this immutable extension.
    pub host_commands: Vec<ExtensionHostCommandDescription>,
    /// Extension-local lifecycle registration IDs.
    pub lifecycle_hook_ids: Vec<String>,
}

/// Metadata-only semantic entry exposed to extension context policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionContextEntry {
    /// Stable durable entry identity.
    pub id: String,
    /// Broad core-owned entry kind.
    pub kind: String,
    /// Whether the entry is eligible for model projection.
    pub model_visible: bool,
    /// Whether the durable runtime will refuse to omit the entry.
    pub protected: bool,
}

/// Bounded metadata-only context view for one extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionContextInput {
    /// Branch entries in durable order.
    pub entries: Vec<ExtensionContextEntry>,
}

/// Bounded annotation proposed for the model context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionContextAnnotation {
    /// Extension-local stable annotation ID.
    pub id: String,
    /// Exact bounded model-visible annotation text.
    pub content: String,
}

/// A metadata-only context projection proposal.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExtensionContextPatch {
    /// Entries to retain; an empty list leaves the host default selection intact.
    pub retain_entries: Vec<String>,
    /// Model-eligible entries to omit only from this request projection.
    pub omit_eligible_entries: Vec<String>,
    /// Annotations appended after selected semantic entries.
    pub annotations: Vec<ExtensionContextAnnotation>,
    /// Model-visible extension-memory entries to select explicitly.
    pub selected_memory: Vec<String>,
    /// A registered compaction strategy requested for host consideration.
    pub requested_compaction_strategy: Option<String>,
}

/// Extension-specific failure that never exposes language-runtime handles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionError {
    /// Bounded host-safe diagnostic.
    pub message: String,
}

impl ExtensionError {
    /// Construct a bounded extension-boundary error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExtensionError {}

/// Metadata-only context projection implemented by an extension adapter.
pub trait ExtensionContextPolicy: Send + Sync {
    /// Propose a bounded context patch from core-owned metadata.
    fn project_context(
        &self,
        input: &ExtensionContextInput,
    ) -> Result<ExtensionContextPatch, ExtensionError>;
}

/// Process-local lifecycle callbacks implemented by an extension adapter.
pub trait ExtensionLifecycle: Send + Sync {
    /// Return extension-local registration IDs in deterministic order.
    fn hook_ids(&self) -> Result<Vec<String>, ExtensionError>;
    /// Propose state committed with a durable operation start.
    fn before_operation(&self) -> Result<BTreeMap<String, JsonValue>, ExtensionError>;
    /// Propose state committed with a durable epoch start.
    fn before_epoch(&self) -> Result<BTreeMap<String, JsonValue>, ExtensionError>;
    /// Rebuild adapter-local state from already committed durable values.
    fn before_resume(
        &self,
        operation_data: &BTreeMap<String, JsonValue>,
        epoch_data: &BTreeMap<String, JsonValue>,
    ) -> Result<(), ExtensionError>;
}

/// Visibility requested for a bounded extension-memory proposal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionMemoryVisibility {
    /// The runtime may project the entry into model context.
    ModelVisible,
    /// The entry remains durable but is not automatically model-visible.
    ExternalOnly,
}

/// Retention requested for a bounded extension-memory proposal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionMemoryRetention {
    /// Retain under ordinary session/export reachability rules.
    Session,
    /// Retain only while a host checkpoint retains the entry.
    Checkpoint,
}

/// One extension-memory proposal awaiting durable runtime validation.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionMemoryProposal {
    /// Extension-defined portable memory kind.
    pub kind: String,
    /// Bounded structured inline content.
    pub content: JsonValue,
    /// Bounded evidence labels.
    pub provenance: Vec<String>,
    /// Requested model visibility.
    pub visibility: ExtensionMemoryVisibility,
    /// Requested retention class.
    pub retention: ExtensionMemoryRetention,
}

/// One proposal paired with the immutable extension that emitted it.
#[derive(Clone, Debug, PartialEq)]
pub struct CollectedExtensionMemoryProposal {
    /// Immutable extension identity.
    pub extension_id: String,
    /// Rust-validated proposal without durable placement information.
    pub proposal: ExtensionMemoryProposal,
}

/// Per-epoch collector joining completed hooks to durable post-tool settlement.
#[derive(Clone, Default)]
pub struct ExtensionMemoryCollector {
    pending: Arc<Mutex<BTreeMap<String, BTreeMap<usize, CollectedExtensionMemoryProposal>>>>,
}

impl fmt::Debug for ExtensionMemoryCollector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pending = self
            .pending
            .lock()
            .map(|values| values.len())
            .unwrap_or_default();
        formatter
            .debug_struct("ExtensionMemoryCollector")
            .field("pending_calls", &pending)
            .finish()
    }
}

impl ExtensionMemoryCollector {
    /// Consume deterministic registry-ordered proposals for one completed tool call.
    pub fn take_for_call(
        &self,
        tool_call_id: &str,
    ) -> Result<Vec<CollectedExtensionMemoryProposal>, ExtensionError> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| ExtensionError::new("extension memory collector lock was poisoned"))?;
        Ok(pending
            .remove(tool_call_id)
            .unwrap_or_default()
            .into_values()
            .collect())
    }

    /// Record at most one proposal from one registry position for a tool call.
    pub fn record(
        &self,
        tool_call_id: &str,
        registry_index: usize,
        proposal: CollectedExtensionMemoryProposal,
    ) -> Result<(), ExtensionError> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| ExtensionError::new("extension memory collector lock was poisoned"))?;
        let proposals = pending.entry(tool_call_id.into()).or_default();
        if proposals.insert(registry_index, proposal).is_some() {
            return Err(ExtensionError::new(format!(
                "extension registration {registry_index} emitted more than one memory proposal for call {tool_call_id}",
            )));
        }
        Ok(())
    }
}

/// Resource ceilings applied to one adapter-owned extension tool invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtensionToolLimits {
    /// Largest accepted handler source in bytes.
    pub max_source_bytes: usize,
    /// Largest language-runtime allocation total in bytes.
    pub max_memory_bytes: usize,
    /// Largest cooperative interruption count per coroutine resume.
    pub max_interrupt_checks: usize,
    /// Largest number of host capability operations per invocation.
    pub max_capability_calls: usize,
}

impl Default for ExtensionToolLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
            max_capability_calls: 64,
        }
    }
}

/// One host capability request yielded by an extension tool.
#[derive(Clone, Debug)]
pub struct ExtensionCapabilityRequest {
    /// Provider call that owns this request.
    pub call_id: ToolCallId,
    /// Model-visible tool that yielded the request.
    pub tool_name: String,
    /// Explicit capability binding selected by the host.
    pub capability: String,
    /// Method interpreted by the bound host object.
    pub method: String,
    /// Parsed JSON arguments supplied by the extension.
    pub arguments: JsonValue,
    /// Host update sink for progress or partial output.
    pub updates: ToolUpdateSink,
}

/// Successful value returned to an extension tool coroutine.
#[derive(Clone, Debug)]
pub struct ExtensionCapabilityResponse {
    /// JSON value supplied to the suspended extension.
    pub value: JsonValue,
}

/// Typed failure at the extension capability boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionCapabilityError {
    /// The owning core run was cancelled.
    Cancelled,
    /// The capability was not explicitly bound.
    NotBound {
        /// Capability requested by the extension.
        capability: String,
    },
    /// The host denied the requested capability method.
    MethodDenied {
        /// Capability that rejected the request.
        capability: String,
        /// Method rejected by that capability.
        method: String,
    },
    /// Host-side argument validation failed.
    InvalidArguments {
        /// Host-safe argument validation diagnostic.
        message: String,
    },
    /// The bound host capability failed.
    Execution {
        /// Host-safe execution diagnostic.
        message: String,
    },
}

impl fmt::Display for ExtensionCapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("extension capability operation was cancelled"),
            Self::NotBound { capability } => write!(
                formatter,
                "capability {capability:?} is not explicitly bound"
            ),
            Self::MethodDenied { capability, method } => write!(
                formatter,
                "capability {capability:?} denied method {method:?}"
            ),
            Self::InvalidArguments { message } => {
                write!(formatter, "invalid capability arguments: {message}")
            }
            Self::Execution { message } => write!(formatter, "capability failed: {message}"),
        }
    }
}

impl std::error::Error for ExtensionCapabilityError {}

/// Caller-polled host capability future.
pub type ExtensionCapabilityFuture = Pin<
    Box<
        dyn Future<Output = Result<ExtensionCapabilityResponse, ExtensionCapabilityError>>
            + Send
            + 'static,
    >,
>;

/// Explicit host capability callable by one resolved extension.
pub trait ExtensionCapability: Send + Sync {
    /// Start one caller-polled capability operation.
    fn invoke(
        &self,
        request: ExtensionCapabilityRequest,
        cancellation: CancellationToken,
    ) -> ExtensionCapabilityFuture;
}

/// One explicit host capability together with its invocation ceilings.
#[derive(Clone)]
pub struct ExtensionCapabilityBinding {
    implementation: Arc<dyn ExtensionCapability>,
    limits: ExtensionToolLimits,
}

impl fmt::Debug for ExtensionCapabilityBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExtensionCapabilityBinding")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ExtensionCapabilityBinding {
    /// Return the host implementation selected for this exact capability.
    pub fn implementation(&self) -> Arc<dyn ExtensionCapability> {
        Arc::clone(&self.implementation)
    }

    /// Return the immutable invocation ceilings selected by the host.
    pub const fn limits(&self) -> ExtensionToolLimits {
        self.limits
    }
}

/// A deterministic ordered set of host capabilities explicitly granted to one extension tool.
#[derive(Clone, Default)]
pub struct ExtensionCapabilityBindings {
    entries: BTreeMap<String, ExtensionCapabilityBinding>,
}

impl fmt::Debug for ExtensionCapabilityBindings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExtensionCapabilityBindings")
            .field("names", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ExtensionCapabilityBindings {
    /// Construct an empty explicit binding set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert one named host capability without permitting replacement.
    pub fn insert(
        &mut self,
        capability: impl Into<String>,
        implementation: Arc<dyn ExtensionCapability>,
        limits: ExtensionToolLimits,
    ) -> Result<(), ExtensionError> {
        let capability = capability.into();
        if capability.trim().is_empty() {
            return Err(ExtensionError::new(
                "capability binding name cannot be empty",
            ));
        }
        if self.entries.contains_key(&capability) {
            return Err(ExtensionError::new(format!(
                "capability {capability:?} is already bound"
            )));
        }
        self.entries.insert(
            capability,
            ExtensionCapabilityBinding {
                implementation,
                limits,
            },
        );
        Ok(())
    }

    /// Look up only a capability explicitly inserted by the host.
    pub fn get(&self, capability: &str) -> Option<ExtensionCapabilityBinding> {
        self.entries.get(capability).cloned()
    }

    /// Iterate explicit capability names in deterministic order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

/// Executable extension artifacts resolved from immutable source for one epoch.
#[derive(Clone)]
pub struct ResolvedExtension {
    /// Hook chain including the caller-provided inner hooks.
    pub hooks: Arc<dyn HookSet>,
    /// Executable model-visible extension tools.
    pub tools: ToolRegistry,
    /// Executable constrained host-command handlers.
    pub host_commands: Vec<Arc<dyn ExtensionHostCommand>>,
    /// Optional post-operation continuation policy.
    pub idle_hook: Option<Arc<dyn ExtensionIdleHook>>,
    /// Optional metadata-only context policy.
    pub context_policy: Option<Arc<dyn ExtensionContextPolicy>>,
    /// Optional process-local lifecycle implementation.
    pub lifecycle: Option<Arc<dyn ExtensionLifecycle>>,
}

impl fmt::Debug for ResolvedExtension {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedExtension")
            .field("tool_count", &self.tools.names().count())
            .field("host_command_count", &self.host_commands.len())
            .field("has_idle_hook", &self.idle_hook.is_some())
            .field("has_context_policy", &self.context_policy.is_some())
            .field("has_lifecycle", &self.lifecycle.is_some())
            .finish_non_exhaustive()
    }
}

/// Language adapter that validates and resolves immutable extension source.
pub trait ExtensionEngine: Send + Sync {
    /// Validate source and derive its provider-visible descriptor.
    fn describe(&self, source: &ExtensionSourceTree)
    -> Result<ExtensionDescriptor, ExtensionError>;

    /// Resolve source into executable core ports for one immutable epoch.
    fn resolve(
        &self,
        source: &ExtensionSourceTree,
        bindings: ExtensionCapabilityBindings,
        inner_hooks: Arc<dyn HookSet>,
        extension_index: usize,
        memory_collector: Arc<ExtensionMemoryCollector>,
    ) -> Result<ResolvedExtension, ExtensionError>;
}

/// Empty engine used when a host deliberately selects no extension implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoExtensions;

impl ExtensionEngine for NoExtensions {
    fn describe(
        &self,
        source: &ExtensionSourceTree,
    ) -> Result<ExtensionDescriptor, ExtensionError> {
        Err(ExtensionError::new(format!(
            "extension {} cannot be resolved because this host installed no extension engine",
            source.extension_id
        )))
    }

    fn resolve(
        &self,
        source: &ExtensionSourceTree,
        _bindings: ExtensionCapabilityBindings,
        _inner_hooks: Arc<dyn HookSet>,
        _extension_index: usize,
        _memory_collector: Arc<ExtensionMemoryCollector>,
    ) -> Result<ResolvedExtension, ExtensionError> {
        Err(ExtensionError::new(format!(
            "extension {} cannot be resolved because this host installed no extension engine",
            source.extension_id
        )))
    }
}

impl From<HookError> for ExtensionError {
    fn from(error: HookError) -> Self {
        Self::new(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::NoHooks;

    struct FakeExtensionEngine;

    impl ExtensionEngine for FakeExtensionEngine {
        fn describe(
            &self,
            source: &ExtensionSourceTree,
        ) -> Result<ExtensionDescriptor, ExtensionError> {
            assert_eq!(source.extension_id, "fake.extension");
            assert_eq!(
                source.expected_capabilities,
                Some(BTreeSet::from(["fake.read".into()]))
            );
            Ok(ExtensionDescriptor {
                requested_capabilities: BTreeSet::from(["fake.read".into()]),
                prompt_sections: Vec::new(),
                tools: Vec::new(),
                host_commands: Vec::new(),
                lifecycle_hook_ids: Vec::new(),
            })
        }

        fn resolve(
            &self,
            source: &ExtensionSourceTree,
            bindings: ExtensionCapabilityBindings,
            inner_hooks: Arc<dyn HookSet>,
            extension_index: usize,
            _memory_collector: Arc<ExtensionMemoryCollector>,
        ) -> Result<ResolvedExtension, ExtensionError> {
            assert_eq!(source.extension_id, "fake.extension");
            assert_eq!(bindings.names().collect::<Vec<_>>(), vec!["fake.read"]);
            assert_eq!(extension_index, 0);
            Ok(ResolvedExtension {
                hooks: inner_hooks,
                tools: ToolRegistry::default(),
                host_commands: Vec::new(),
                idle_hook: None,
                context_policy: None,
                lifecycle: None,
            })
        }
    }

    #[test]
    fn fake_engine_exercises_the_core_owned_extension_contract() {
        let source = ExtensionSourceTree {
            extension_id: "fake.extension".into(),
            files: BTreeMap::from([("entry.fake".into(), "return {}".into())]),
            expected_capabilities: Some(BTreeSet::from(["fake.read".into()])),
            limits: ExtensionLimits {
                max_source_bytes: 1024,
                max_memory_bytes: 1024,
                max_interrupt_checks: 10,
            },
        };
        let engine = FakeExtensionEngine;
        assert_eq!(
            engine
                .describe(&source)
                .expect("fake engine describes source")
                .requested_capabilities,
            BTreeSet::from(["fake.read".into()]),
        );

        let mut bindings = ExtensionCapabilityBindings::new();
        bindings
            .insert(
                "fake.read",
                Arc::new(UnreachableCapability),
                ExtensionToolLimits::default(),
            )
            .expect("one explicit fake capability binds");
        let resolved = engine
            .resolve(
                &source,
                bindings,
                Arc::new(NoHooks),
                0,
                Arc::new(ExtensionMemoryCollector::default()),
            )
            .expect("fake engine resolves source");
        assert_eq!(resolved.tools.names().count(), 0);
    }

    struct UnreachableCapability;

    impl ExtensionCapability for UnreachableCapability {
        fn invoke(
            &self,
            _request: ExtensionCapabilityRequest,
            _cancellation: CancellationToken,
        ) -> ExtensionCapabilityFuture {
            Box::pin(async {
                Err(ExtensionCapabilityError::Execution {
                    message: "test capability must not be invoked".into(),
                })
            })
        }
    }
}
