//! Immutable harness-revision resolution and activation validation.
//!
//! The manager is deliberately narrower than the durable supervisor.  It
//! owns immutable lineage objects and turns a committed revision into a fresh
//! provider-independent resolved harness, but it has no session writer and cannot change
//! a lane by itself.  The supervisor remains responsible for writing a
//! `HarnessRevisionChanged` semantic entry at an epoch boundary.

use crate::harness::lineage::{compose_system_prompt, runtime_hook_bundle_digest};
use crate::harness::{
    CandidateHypothesis, HarnessActor, HarnessCandidateDraft, HarnessCandidateV1, HarnessError,
    HarnessLineageError, HarnessRepository, HarnessRevisionV1, HarnessSnapshotV1,
    HarnessSourceFile, HarnessSurface, HarnessTreeLimits, PluginBundleRef, PluginCapabilityCatalog,
    RegistryOperation, SelfExtensionMode,
};
use crate::runtime::context::ContextPolicyRegistry;
use crate::runtime::lifecycle::PluginLifecycleRegistry;
use crate::runtime::{HarnessIdentity, RuntimeServices};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use tea_core::compaction::AutomaticCompactionPolicy;
use tea_core::harness::extension::{
    ExtensionEngine, ExtensionMemoryCollector, ExtensionSourceTree,
};
use tea_core::hooks::HookSet;
use tea_core::tool::{ToolFailureCircuitBreaker, ToolRegistry, ToolResultProjectionPolicy};
use tea_protocol::JsonValue;
use tea_session::{
    ArtifactId, ArtifactPolicy, ArtifactStore, HarnessCandidateId, HarnessCatalogFact,
    HarnessRevisionId, NormalizedPath, OperationId, SessionFact, SessionWriter,
};

/// Fixed artifact media type for immutable harness repository catalogs.
pub(crate) const HARNESS_CATALOG_MEDIA_TYPE: &str = "application/vnd.tea.harness-catalog+json";
const HARNESS_CATALOG_SCHEMA_VERSION: u16 = 1;

/// Verify a durable catalog against an explicit extension implementation.
pub fn verify_harness_catalog_with_extension_engine(
    fact: &HarnessCatalogFact,
    artifacts: Arc<dyn ArtifactStore>,
    extension_engine: Arc<dyn ExtensionEngine>,
) -> Result<BTreeSet<ArtifactId>, HarnessLineageError> {
    let catalog = decode_harness_catalog(fact, artifacts, extension_engine)?;
    Ok(catalog.repository.artifact_roots())
}

struct DecodedHarnessCatalog {
    capability_ceiling: BTreeSet<String>,
    repository: HarnessRepository,
}

/// One guarded atomic source edit accepted by the stable `tea_harness` host
/// tool.  A delete names the currently visible immutable blob so stale or
/// destructive requests cannot silently remove newer source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessFilePatch {
    /// Create or replace one UTF-8 manifest or Luau source file.
    Upsert {
        /// Canonical plugin-relative tree path.
        path: NormalizedPath,
        /// Exact replacement source text.
        content: String,
    },
    /// Remove one source file only if its content identity still matches.
    Delete {
        /// Canonical plugin-relative tree path.
        path: NormalizedPath,
        /// Existing content-addressed source identity required for deletion.
        expected_artifact_id: ArtifactId,
    },
}

/// Model-independent input for atomically staging a session-local candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessApplyRequest {
    /// Revision the patch was authored against; stale bases are rejected.
    pub base_revision_id: HarnessRevisionId,
    /// Required evidence/effect/risk declaration.
    pub hypothesis: CandidateHypothesis,
    /// All source mutations applied atomically before validation.
    pub files: Vec<HarnessFilePatch>,
    /// Structured session-plugin registry changes.
    pub registry_operations: Vec<RegistryOperation>,
    /// Current durable operation, retained only for candidate provenance.
    pub operation_id: Option<OperationId>,
    /// Stable model-tool invocation identity for idempotent retries.
    pub tool_invocation_id: String,
}

/// Immutable executable configuration selected for one core epoch.
///
/// The template is cloned while resolving the revision.  Once an epoch starts
/// it owns this value and cannot observe a subsequent candidate activation.
#[derive(Clone)]
pub struct ResolvedHarness {
    /// Exact immutable attribution for the epoch.
    pub identity: HarnessIdentity,
    /// Immutable system instructions derived from the selected snapshot.
    pub(crate) system_prompt: String,
    /// Source-pinned executable extension tools. Trusted base tools remain in
    /// `RuntimeServices` and are combined only while constructing an agent.
    pub(crate) extension_tools: ToolRegistry,
    /// Immutable extension commands retained separately from provider tools.
    pub(crate) host_commands: Vec<ResolvedHostCommand>,
    /// Optional extension callbacks evaluated only at a durable idle boundary.
    pub(crate) idle_hooks: Vec<ResolvedIdleHook>,
    /// Host hooks wrapped by source-pinned extension hooks for this snapshot.
    pub(crate) hooks: Arc<dyn HookSet>,
    /// Immutable policy values selected for this resolved epoch.
    pub(crate) automatic_compaction: AutomaticCompactionPolicy,
    pub(crate) tool_result_projection: ToolResultProjectionPolicy,
    pub(crate) tool_failure_circuit_breaker: ToolFailureCircuitBreaker,
    pub(crate) replay_safe_tools: BTreeSet<String>,
    pub(crate) artifact_policy: ArtifactPolicy,
    /// Frozen session-level self-extension exposure for this epoch.
    pub self_extension_mode: SelfExtensionMode,
    /// Fresh source-pinned, capability-free lifecycle policies for this
    /// immutable snapshot. The field stays crate-private because only the
    /// durable supervisor may consume or persist their output.
    pub(crate) lifecycle: PluginLifecycleRegistry,
    /// Process-local post-tool memory proposals emitted by the exact policy
    /// VMs used for this epoch. The supervisor consumes these only after raw
    /// tool evidence has committed.
    pub(crate) memory_collector: Arc<ExtensionMemoryCollector>,
    /// The immutable source/configuration snapshot from which this executable
    /// configuration was resolved. The supervisor uses it only to derive the
    /// model context against the same pinned provider surface; it is never a
    /// mutable active pointer.
    pub(crate) harness_snapshot: Option<HarnessSnapshotV1>,
    /// Deterministic source-pinned metadata-only context policies. The
    /// supervisor invokes these during context derivation and validates their
    /// output before a provider request is constructed.
    pub(crate) context_policies: ContextPolicyRegistry,
}

/// One immutable source-identity change between two revision views.
///
/// The control tool exposes this metadata instead of unbounded source text so
/// an agent can decide which exact file to read next without treating a diff
/// response as a second mutable worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HarnessSourceDiff {
    pub(crate) path: NormalizedPath,
    pub(crate) before: Option<ArtifactId>,
    pub(crate) after: Option<ArtifactId>,
}

impl std::fmt::Debug for ResolvedHarness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedHarness")
            .field("identity", &self.identity)
            .field("self_extension_mode", &self.self_extension_mode)
            .finish_non_exhaustive()
    }
}

impl ResolvedHarness {
    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub(crate) fn extension_tools(&self) -> &ToolRegistry {
        &self.extension_tools
    }

    pub(crate) fn host_commands(&self) -> &[ResolvedHostCommand] {
        &self.host_commands
    }

    pub(crate) fn idle_hooks(&self) -> &[ResolvedIdleHook] {
        &self.idle_hooks
    }

    pub(crate) fn hooks(&self) -> Arc<dyn HookSet> {
        Arc::clone(&self.hooks)
    }

    pub(crate) fn automatic_compaction_policy(&self) -> &AutomaticCompactionPolicy {
        &self.automatic_compaction
    }

    pub(crate) fn tool_result_projection_policy(&self) -> &ToolResultProjectionPolicy {
        &self.tool_result_projection
    }

    pub(crate) fn tool_failure_circuit_breaker(&self) -> ToolFailureCircuitBreaker {
        self.tool_failure_circuit_breaker
    }

    pub(crate) fn is_replay_safe(&self, name: &str) -> bool {
        self.replay_safe_tools.contains(name)
    }

    pub(crate) fn artifact_policy_config(&self) -> &ArtifactPolicy {
        &self.artifact_policy
    }
}

/// One executable command paired with the extension that owns its namespace.
#[derive(Clone)]
pub(crate) struct ResolvedHostCommand {
    pub(crate) extension_id: String,
    pub(crate) command: Arc<dyn tea_core::harness::extension::ExtensionHostCommand>,
}

/// One executable idle hook paired with the extension whose state it may read.
#[derive(Clone)]
pub(crate) struct ResolvedIdleHook {
    pub(crate) extension_id: String,
    pub(crate) hook: Arc<dyn tea_core::harness::extension::ExtensionIdleHook>,
}

/// Session-local immutable harness catalog.
///
/// A host initializes it from the globally pinned source inputs that belong to
/// one session.  It can stage and validate candidates, but activation returns
/// an immutable revision only; a caller must still commit that revision to the
/// session branch through the durable activation protocol.
pub struct HarnessResolver {
    repository: Mutex<HarnessRepository>,
    capability_ceiling: BTreeSet<String>,
    capability_catalog: PluginCapabilityCatalog,
    extension_engine: Arc<dyn ExtensionEngine>,
    tree_limits: HarnessTreeLimits,
    self_extension_mode: SelfExtensionMode,
    reserved_command_names: BTreeSet<String>,
}

impl std::fmt::Debug for HarnessResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessResolver")
            .field("capability_ceiling", &self.capability_ceiling)
            .field("capability_catalog", &self.capability_catalog)
            .field("tree_limits", &self.tree_limits)
            .field("self_extension_mode", &self.self_extension_mode)
            .finish_non_exhaustive()
    }
}

impl HarnessResolver {
    /// Create a session-local catalog from immutable source/snapshot lineage
    /// and the trusted base executable capabilities.
    pub fn new(repository: HarnessRepository, capability_ceiling: BTreeSet<String>) -> Self {
        let extension_engine = repository.extension_engine();
        Self {
            repository: Mutex::new(repository),
            capability_ceiling,
            capability_catalog: PluginCapabilityCatalog::default(),
            extension_engine,
            tree_limits: HarnessTreeLimits::default(),
            self_extension_mode: SelfExtensionMode::Off,
            reserved_command_names: BTreeSet::new(),
        }
    }

    /// Select the trusted session-level self-extension exposure mode before
    /// the manager is shared with a durable supervisor.
    pub fn self_extension_mode(mut self, mode: SelfExtensionMode) -> Self {
        self.self_extension_mode = mode;
        self
    }

    /// Install the host-owned capability catalog that may adapt validated
    /// plugin handler source into executable core tools. A catalog is fixed
    /// before the manager is shared; candidates can request only the names
    /// already represented by their immutable snapshot bindings.
    pub fn capability_catalog(mut self, catalog: PluginCapabilityCatalog) -> Self {
        self.capability_catalog = catalog;
        self
    }

    /// Reserve native host command names before extensions resolve. A collision
    /// is a deterministic resolution failure, never registration-order luck.
    pub fn reserved_extension_command_names<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.reserved_command_names = names.into_iter().map(Into::into).collect();
        self
    }

    /// Return the fixed mode chosen by the session host.
    pub const fn self_extension_mode_value(&self) -> SelfExtensionMode {
        self.self_extension_mode
    }

    /// Resolve one already staged revision into an immutable core-epoch
    /// configuration.  Source validation occurred while staging its snapshot;
    /// this method still fail-closes if lineage metadata is absent.
    pub fn resolve_revision(
        &self,
        revision_id: &HarnessRevisionId,
        runtime_services: &RuntimeServices,
    ) -> Result<ResolvedHarness, HarnessError> {
        let (revision, snapshot, extensions) = {
            let repository = self.lock_repository()?;
            let revision = repository.revision(revision_id).cloned().ok_or_else(|| {
                HarnessError::invalid_state(format!("unknown harness revision {revision_id}"))
            })?;
            let snapshot = repository
                .snapshot(&revision.snapshot_id)
                .cloned()
                .ok_or_else(|| {
                    HarnessError::invalid_state(format!(
                        "revision {revision_id} references missing snapshot {}",
                        revision.snapshot_id
                    ))
                })?;
            let extensions = repository
                .load_extension_sources(&snapshot)
                .map_err(lineage_error)?;
            (revision, snapshot, extensions)
        };
        let mut resolved_bindings = BTreeMap::new();
        for loaded in &extensions {
            self.validate_fixed_tool_authority(&loaded.source)?;
            for capability in &loaded.plugin.requested_capabilities {
                let reference = snapshot
                    .spec
                    .capability_bindings
                    .iter()
                    .find(|reference| {
                        reference.plugin_id == loaded.plugin.plugin_id
                            && reference.capability == *capability
                    })
                    .ok_or_else(|| HarnessError::invalid_state(format!(
                        "revision {revision_id} plugin {} requests capability {capability} without an immutable host binding",
                        loaded.plugin.plugin_id,
                    )))?;
                let binding = self.capability_catalog.bind(
                    &loaded.plugin.plugin_id,
                    capability,
                    &reference.capability_version,
                    reference.binding_digest,
                    &snapshot.id,
                    &snapshot.spec.resource_limits,
                )?;
                resolved_bindings.insert(
                    (loaded.plugin.plugin_id.clone(), capability.clone()),
                    binding,
                );
            }
        }
        if self.self_extension_mode.exposes_control_tool()
            && snapshot
                .spec
                .self_extension_addendum
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(HarnessError::invalid_state(
                "an enabled self-extension mode requires a pinned stable addendum in its snapshot",
            ));
        }
        if self.self_extension_mode == SelfExtensionMode::Off
            && snapshot.spec.self_extension_addendum.is_some()
        {
            return Err(HarnessError::invalid_state(
                "an off self-extension mode cannot resolve a snapshot with an authoring addendum",
            ));
        }
        let memory_collector = Arc::new(ExtensionMemoryCollector::default());
        let mut hooks = runtime_services.base_hook_set();
        let mut plugin_tools = ToolRegistry::default();
        let mut resolved_extensions = Vec::new();
        for (index, loaded) in extensions.iter().enumerate().rev() {
            let mut bindings = tea_core::harness::extension::ExtensionCapabilityBindings::new();
            for capability in &loaded.plugin.requested_capabilities {
                let binding = resolved_bindings
                    .get(&(loaded.plugin.plugin_id.clone(), capability.clone()))
                    .ok_or_else(|| {
                        HarnessError::invalid_state(format!(
                            "plugin {} capability {capability} has no resolved host binding",
                            loaded.plugin.plugin_id,
                        ))
                    })?;
                let capability_binding = binding.capabilities.get(capability).ok_or_else(|| {
                    HarnessError::invalid_state(format!(
                        "plugin {} capability {capability} was not retained by its snapshot binding",
                        loaded.plugin.plugin_id,
                    ))
                })?;
                bindings
                    .insert(
                        capability.clone(),
                        capability_binding.implementation(),
                        capability_binding.limits(),
                    )
                    .map_err(|error| {
                        HarnessError::invalid_state(format!(
                            "plugin {} capability {capability} could not bind: {error}",
                            loaded.plugin.plugin_id,
                        ))
                    })?;
            }
            if let Some((tool_capabilities, additional_read_only_capabilities)) = self
                .capability_catalog
                .fixed_tool_capabilities(&loaded.plugin.plugin_id)
            {
                bindings
                    .fix_tool_capabilities(
                        tool_capabilities.clone(),
                        additional_read_only_capabilities.clone(),
                    )
                    .map_err(|error| {
                        HarnessError::invalid_state(format!(
                            "plugin {} could not fix tool capability grants: {error}",
                            loaded.plugin.plugin_id,
                        ))
                    })?;
            }
            let resolved = self
                .extension_engine
                .resolve(
                    &loaded.source,
                    bindings,
                    Arc::clone(&hooks),
                    index,
                    Arc::clone(&memory_collector),
                )
                .map_err(|error| {
                    HarnessError::invalid_state(format!(
                        "plugin {} could not resolve its extension runtime: {error}",
                        loaded.plugin.plugin_id,
                    ))
                })?;
            for name in resolved.tools.names() {
                if plugin_tools.get(name).is_some() {
                    return Err(HarnessError::invalid_state(format!(
                        "plugins declare duplicate executable tool {name}",
                    )));
                }
            }
            for name in resolved
                .tools
                .names()
                .map(str::to_owned)
                .collect::<Vec<_>>()
            {
                let tool = resolved
                    .tools
                    .get(&name)
                    .expect("resolved extension tool remains registered")
                    .clone();
                plugin_tools.insert(tool);
            }
            hooks = Arc::clone(&resolved.hooks);
            resolved_extensions.push((loaded.plugin.plugin_id.clone(), resolved));
        }
        resolved_extensions.reverse();
        let lifecycle =
            PluginLifecycleRegistry::from_resolved(resolved_extensions.iter().filter_map(
                |(plugin_id, resolved)| {
                    resolved
                        .lifecycle
                        .as_ref()
                        .map(|lifecycle| (plugin_id.clone(), Arc::clone(lifecycle)))
                },
            ))?;
        let context_policies =
            ContextPolicyRegistry::from_resolved(resolved_extensions.iter().filter_map(
                |(plugin_id, resolved)| {
                    resolved
                        .context_policy
                        .as_ref()
                        .map(|policy| (plugin_id.clone(), Arc::clone(policy)))
                },
            ));
        let mut command_names = BTreeSet::new();
        let mut host_commands = Vec::new();
        let mut idle_hooks = Vec::new();
        for (plugin_id, resolved) in &resolved_extensions {
            for command in &resolved.host_commands {
                let description = command.description();
                if self.reserved_command_names.contains(&description.name) {
                    return Err(HarnessError::invalid_state(format!(
                        "plugin {plugin_id} command {} collides with a native host command",
                        description.name
                    )));
                }
                if !command_names.insert(description.name.clone()) {
                    return Err(HarnessError::invalid_state(format!(
                        "plugins declare duplicate host command {}",
                        description.name
                    )));
                }
                host_commands.push(ResolvedHostCommand {
                    extension_id: plugin_id.clone(),
                    command: Arc::clone(command),
                });
            }
            if let Some(hook) = &resolved.idle_hook {
                idle_hooks.push(ResolvedIdleHook {
                    extension_id: plugin_id.clone(),
                    hook: Arc::clone(hook),
                });
            }
        }
        resolve_snapshot(ResolveSnapshotInput {
            runtime_services,
            revision: &revision,
            snapshot: &snapshot,
            self_extension_mode: self.self_extension_mode,
            hooks,
            plugin_tools,
            host_commands,
            idle_hooks,
            lifecycle,
            memory_collector,
            context_policies,
        })
    }

    /// Stage one immutable candidate.  Candidate validation remains fully
    /// deterministic and capability expansion is compared to this frozen
    /// session ceiling, never to model-controlled source text.
    pub fn stage_candidate(
        &self,
        mut draft: HarnessCandidateDraft,
    ) -> Result<HarnessCandidateV1, HarnessError> {
        draft.capability_ceiling = self.capability_ceiling.clone();
        self.lock_repository()?
            .stage_candidate(draft)
            .map_err(lineage_error)
    }

    /// Stage one full candidate from an atomic source patch and structured
    /// registry operations.  This method never changes an active revision;
    /// it produces immutable source/tree/snapshot objects first and returns a
    /// candidate whose validation result describes whether activation is
    /// permitted.
    pub fn apply(
        &self,
        request: HarnessApplyRequest,
        runtime_services: &RuntimeServices,
    ) -> Result<HarnessCandidateV1, HarnessError> {
        if request.tool_invocation_id.trim().is_empty() {
            return Err(HarnessError::invalid_state(
                "harness apply requires a stable tool invocation identity",
            ));
        }
        let mut repository = self.lock_repository()?;
        let parent_revision = repository
            .revision(&request.base_revision_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "harness apply base revision {} does not exist",
                    request.base_revision_id
                ))
            })?;
        let parent_snapshot = repository
            .snapshot(&parent_revision.snapshot_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "harness apply base revision {} has no immutable snapshot",
                    request.base_revision_id
                ))
            })?;
        let target_plugin_ids =
            apply_registry_operations(&parent_snapshot, &request.registry_operations)?;
        let global_plugin_ids = parent_snapshot
            .spec
            .ordered_global_plugins
            .iter()
            .map(|plugin| plugin.plugin_id.clone())
            .collect::<BTreeSet<_>>();
        let current_plugin_ids = parent_snapshot
            .spec
            .ordered_session_plugins
            .iter()
            .map(|plugin| plugin.plugin_id.clone())
            .collect::<BTreeSet<_>>();
        let added_plugin_ids = target_plugin_ids
            .iter()
            .filter(|plugin_id| !current_plugin_ids.contains(*plugin_id))
            .cloned()
            .collect::<BTreeSet<_>>();
        let target_plugin_id_set = target_plugin_ids.iter().cloned().collect::<BTreeSet<_>>();
        let removed_plugin_ids = current_plugin_ids
            .difference(&target_plugin_id_set)
            .cloned()
            .collect::<BTreeSet<_>>();

        let mut files = collect_session_source_files(&repository, &parent_snapshot)?;
        apply_source_patches(
            &mut files,
            &request.files,
            &current_plugin_ids,
            &added_plugin_ids,
            &global_plugin_ids,
        )?;
        files.retain(|path, _| {
            plugin_id_from_path(path)
                .is_some_and(|plugin_id| !removed_plugin_ids.contains(plugin_id))
        });

        let mut proposed_spec = parent_snapshot.spec.clone();
        let resource_limits = proposed_spec.resource_limits.clone();
        proposed_spec.ordered_session_plugins = if target_plugin_ids.is_empty() {
            Vec::new()
        } else {
            let tree = repository
                .stage_tree(
                    files
                        .into_iter()
                        .map(|(path, file)| (path, file.bytes, file.media_type)),
                    &self.tree_limits,
                )
                .map_err(lineage_error)?;
            target_plugin_ids
                .iter()
                .map(|plugin_id| {
                    Ok(PluginBundleRef {
                        plugin_id: plugin_id.clone(),
                        tree_id: tree.id.clone(),
                        requested_capabilities: repository
                            .plugin_capabilities(&tree.id, plugin_id, &resource_limits)
                            .map_err(lineage_error)?,
                    })
                })
                .collect::<Result<Vec<_>, HarnessError>>()?
        };
        proposed_spec.hook_bundle_digest = runtime_hook_bundle_digest(
            runtime_services
                .runtime_policy_identities()
                .hook_bundle_digest,
            &proposed_spec,
        );
        let proposed_snapshot = repository
            .stage_snapshot(proposed_spec)
            .map_err(lineage_error)?;
        self.validate_fixed_tool_authority_for_snapshot(&repository, &proposed_snapshot)?;
        let changed_surfaces = changed_surfaces(&parent_snapshot, &proposed_snapshot);
        let draft = HarnessCandidateDraft {
            parent_revision_id: request.base_revision_id,
            proposed_snapshot_id: proposed_snapshot.id,
            actor: HarnessActor::Model,
            operation_id: request.operation_id,
            tool_invocation_id: Some(request.tool_invocation_id),
            hypothesis: request.hypothesis.clone(),
            changed_paths: request
                .files
                .iter()
                .map(|patch| match patch {
                    HarnessFilePatch::Upsert { path, .. }
                    | HarnessFilePatch::Delete { path, .. } => path.clone(),
                })
                .collect(),
            registry_operations: request.registry_operations,
            changed_surfaces,
            targeted_failures: vec![request.hypothesis.targeted_evidence],
            evidence: Vec::new(),
            expected_effects: vec![request.hypothesis.expected_effect],
            regression_risks: vec![request.hypothesis.regression_risk],
            capability_ceiling: self.capability_ceiling.clone(),
        };
        repository.stage_candidate(draft).map_err(lineage_error)
    }

    /// Verify any host-pinned model-tool authority map against modifiable
    /// source before a candidate becomes addressable. This keeps first-party
    /// coding behavior revisionable without making its tool-to-capability
    /// mapping source-controlled.
    fn validate_fixed_tool_authority_for_snapshot(
        &self,
        repository: &HarnessRepository,
        snapshot: &HarnessSnapshotV1,
    ) -> Result<(), HarnessError> {
        for loaded in repository
            .load_extension_sources(snapshot)
            .map_err(lineage_error)?
        {
            self.validate_fixed_tool_authority(&loaded.source)?;
        }
        Ok(())
    }

    fn validate_fixed_tool_authority(
        &self,
        source: &ExtensionSourceTree,
    ) -> Result<(), HarnessError> {
        let Some((fixed, additional_read_only_capabilities)) = self
            .capability_catalog
            .fixed_tool_capabilities(&source.extension_id)
        else {
            return Ok(());
        };
        let descriptor = self.extension_engine.describe(source).map_err(|error| {
            HarnessError::invalid_state(format!(
                "plugin {} could not describe its fixed tool authority: {error}",
                source.extension_id,
            ))
        })?;
        let actual = descriptor
            .tools
            .into_iter()
            .map(|tool| (tool.name, tool.capability))
            .collect::<BTreeMap<_, _>>();
        let fixed_mismatch = fixed
            .iter()
            .any(|(tool, capability)| actual.get(tool) != Some(capability));
        let authority_expansion = actual.iter().any(|(tool, capability)| {
            !fixed.contains_key(tool) && !additional_read_only_capabilities.contains(capability)
        });
        if fixed_mismatch || authority_expansion {
            return Err(HarnessError::invalid_state(format!(
                "plugin {} tool capability declarations differ from the host-fixed authority map",
                source.extension_id,
            )));
        }
        Ok(())
    }

    /// Recheck and derive the immutable child revision for a staged candidate.
    ///
    /// This is idempotent: a recovery retry returns the same revision identity
    /// and preserves the original staged metadata instead of creating a second
    /// active pointer.
    pub fn activate_candidate(
        &self,
        candidate_id: &HarnessCandidateId,
        actor: HarnessActor,
        created_at_ms: u64,
    ) -> Result<HarnessRevisionV1, HarnessError> {
        self.lock_repository()?
            .activate_candidate(candidate_id, actor, created_at_ms)
            .map_err(lineage_error)
    }

    /// Return a cloned immutable candidate for read-only status and recovery
    /// validation.
    pub fn candidate(
        &self,
        candidate_id: &HarnessCandidateId,
    ) -> Result<HarnessCandidateV1, HarnessError> {
        self.lock_repository()?
            .candidate(candidate_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!("unknown harness candidate {candidate_id}"))
            })
    }

    /// Return a cloned immutable revision for status or durable activation.
    pub fn revision(
        &self,
        revision_id: &HarnessRevisionId,
    ) -> Result<HarnessRevisionV1, HarnessError> {
        self.lock_repository()?
            .revision(revision_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!("unknown harness revision {revision_id}"))
            })
    }

    /// Return a cloned snapshot for read-only status and diffing.
    pub fn snapshot_for_revision(
        &self,
        revision_id: &HarnessRevisionId,
    ) -> Result<HarnessSnapshotV1, HarnessError> {
        let repository = self.lock_repository()?;
        let revision = repository.revision(revision_id).ok_or_else(|| {
            HarnessError::invalid_state(format!("unknown harness revision {revision_id}"))
        })?;
        repository
            .snapshot(&revision.snapshot_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "revision {revision_id} references missing snapshot {}",
                    revision.snapshot_id
                ))
            })
    }

    /// Return a cloned immutable snapshot by content identity for read-only
    /// candidate inspection and exact diffing.
    pub fn snapshot(
        &self,
        snapshot_id: &tea_session::HarnessSnapshotId,
    ) -> Result<HarnessSnapshotV1, HarnessError> {
        self.lock_repository()?
            .snapshot(snapshot_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!("unknown harness snapshot {snapshot_id}",))
            })
    }

    /// Return immutable source-artifact roots retained by the session's
    /// harness lineage. Hosts pass these alongside direct session roots to
    /// `tea_session::plan_artifact_gc`; omitting them would make a catalog
    /// reconstructible in name but lose its exact source bytes.
    pub fn artifact_roots(&self) -> Result<BTreeSet<ArtifactId>, HarnessError> {
        Ok(self.lock_repository()?.artifact_roots())
    }

    /// Return immutable revisions in canonical identity order for the bounded
    /// control-tool list view.
    pub(crate) fn revisions(&self) -> Result<Vec<HarnessRevisionV1>, HarnessError> {
        Ok(self.lock_repository()?.revisions().cloned().collect())
    }

    /// Return immutable candidates in canonical identity order for the
    /// bounded control-tool list view.
    pub(crate) fn candidates(&self) -> Result<Vec<HarnessCandidateV1>, HarnessError> {
        Ok(self.lock_repository()?.candidates().cloned().collect())
    }

    /// Read one exact source file that belongs to the selected immutable
    /// revision.  The result is a clone of a content-addressed object, never
    /// a mutable file-system path.
    pub(crate) fn read_source(
        &self,
        revision_id: &HarnessRevisionId,
        path: &NormalizedPath,
    ) -> Result<HarnessSourceFile, HarnessError> {
        let repository = self.lock_repository()?;
        let snapshot = snapshot_for_revision(&repository, revision_id)?;
        source_files_for_snapshot(&repository, snapshot)?
            .remove(path)
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "revision {revision_id} does not contain immutable source {path}",
                ))
            })
    }

    /// Compare exact source object identities between two immutable revisions.
    /// Content is deliberately not embedded in this metadata view; callers
    /// request a bounded page through [`Self::read_source`] when needed.
    pub(crate) fn diff_revisions(
        &self,
        before_revision_id: &HarnessRevisionId,
        after_revision_id: &HarnessRevisionId,
    ) -> Result<Vec<HarnessSourceDiff>, HarnessError> {
        let repository = self.lock_repository()?;
        let before = source_files_for_snapshot(
            &repository,
            snapshot_for_revision(&repository, before_revision_id)?,
        )?;
        let after = source_files_for_snapshot(
            &repository,
            snapshot_for_revision(&repository, after_revision_id)?,
        )?;
        let paths = before
            .keys()
            .chain(after.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        Ok(paths
            .into_iter()
            .filter_map(|path| {
                let before_id = before.get(&path).map(|file| file.artifact_id);
                let after_id = after.get(&path).map(|file| file.artifact_id);
                (before_id != after_id).then_some(HarnessSourceDiff {
                    path,
                    before: before_id,
                    after: after_id,
                })
            })
            .collect())
    }

    /// Stage an immutable rollback candidate that selects an ancestor's
    /// snapshot through the ordinary candidate/activation protocol.  It does
    /// not mutate the current branch or destructively replace any source.
    pub(crate) fn stage_rollback(
        &self,
        base_revision_id: HarnessRevisionId,
        target_revision_id: HarnessRevisionId,
        hypothesis: CandidateHypothesis,
        operation_id: Option<OperationId>,
        tool_invocation_id: String,
    ) -> Result<HarnessCandidateV1, HarnessError> {
        if tool_invocation_id.trim().is_empty() {
            return Err(HarnessError::invalid_state(
                "harness rollback requires a stable tool invocation identity",
            ));
        }
        let mut repository = self.lock_repository()?;
        let parent = repository
            .revision(&base_revision_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "harness rollback base revision {base_revision_id} does not exist",
                ))
            })?;
        let target = repository
            .revision(&target_revision_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "harness rollback target revision {target_revision_id} does not exist",
                ))
            })?;
        if base_revision_id == target_revision_id {
            return Err(HarnessError::invalid_state(
                "harness rollback target must be an earlier immutable revision",
            ));
        }
        if !revision_is_ancestor(&repository, &target_revision_id, &base_revision_id)? {
            return Err(HarnessError::invalid_state(format!(
                "harness rollback target {target_revision_id} is not an ancestor of {base_revision_id}",
            )));
        }
        let parent_snapshot = repository
            .snapshot(&parent.snapshot_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "harness rollback base revision {base_revision_id} has no immutable snapshot",
                ))
            })?;
        let target_snapshot = repository
            .snapshot(&target.snapshot_id)
            .cloned()
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "harness rollback target revision {target_revision_id} has no immutable snapshot",
            )))?;
        let draft = HarnessCandidateDraft {
            parent_revision_id: base_revision_id,
            proposed_snapshot_id: target.snapshot_id,
            actor: HarnessActor::Model,
            operation_id,
            tool_invocation_id: Some(tool_invocation_id),
            hypothesis: hypothesis.clone(),
            changed_paths: Vec::new(),
            registry_operations: Vec::new(),
            changed_surfaces: changed_surfaces(&parent_snapshot, &target_snapshot),
            targeted_failures: vec![hypothesis.targeted_evidence],
            evidence: Vec::new(),
            expected_effects: vec![hypothesis.expected_effect],
            regression_risks: vec![hypothesis.regression_risk],
            capability_ceiling: self.capability_ceiling.clone(),
        };
        repository.stage_candidate(draft).map_err(lineage_error)
    }

    /// Return the fixed capability ceiling selected when this session began.
    pub fn capability_ceiling(&self) -> &BTreeSet<String> {
        &self.capability_ceiling
    }

    /// Persist the complete immutable catalog before a caller records an
    /// activation obligation or branch revision that refers to it. The write
    /// is idempotent when the latest committed catalog already names the
    /// exact same content-addressed manifest.
    pub(crate) fn persist_catalog<S>(
        &self,
        session: &mut S,
        artifacts: &dyn ArtifactStore,
    ) -> Result<(), HarnessError>
    where
        S: SessionWriter,
    {
        let bytes = self.catalog_bytes()?;
        let descriptor = artifacts
            .put(&bytes, HARNESS_CATALOG_MEDIA_TYPE)
            .map_err(|error| {
                HarnessError::invalid_state(format!(
                    "could not persist immutable harness catalog: {error}"
                ))
            })?;
        let fact = HarnessCatalogFact {
            schema_version: HARNESS_CATALOG_SCHEMA_VERSION,
            artifact_id: descriptor.artifact_id,
            byte_len: descriptor.byte_len,
        };
        let snapshot = session.snapshot()?;
        let already_current = snapshot.facts().last().is_some_and(|stored| {
            matches!(&stored.fact, SessionFact::HarnessCatalog(existing) if existing == &fact)
        });
        if !already_current {
            session.append_fact(SessionFact::HarnessCatalog(fact))?;
        }
        Ok(())
    }

    /// Replace the in-memory repository index from the latest durable catalog
    /// artifact. The caller supplies the session-owned artifact store so a
    /// missing or altered source blob faults closed before a revision can be
    /// resolved for a new core epoch.
    pub(crate) fn restore_catalog(
        &self,
        fact: &HarnessCatalogFact,
        artifacts: std::sync::Arc<dyn ArtifactStore>,
    ) -> Result<(), HarnessError> {
        let catalog = decode_harness_catalog(fact, artifacts, Arc::clone(&self.extension_engine))
            .map_err(lineage_error)?;
        if catalog.capability_ceiling != self.capability_ceiling {
            return Err(HarnessError::invalid_state(
                "durable harness catalog capability ceiling does not match the trusted session manager configuration",
            ));
        }
        *self.lock_repository()? = catalog.repository;
        Ok(())
    }

    fn catalog_bytes(&self) -> Result<Vec<u8>, HarnessError> {
        let repository = self.lock_repository()?;
        let payload = JsonValue::object([
            (
                "schema_version",
                JsonValue::from(u64::from(HARNESS_CATALOG_SCHEMA_VERSION)),
            ),
            (
                "capability_ceiling",
                JsonValue::Array(
                    self.capability_ceiling
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            ),
            (
                "repository",
                repository.catalog_json().map_err(lineage_error)?,
            ),
        ]);
        payload
            .to_json_string()
            .map(|text| text.into_bytes())
            .map_err(|error| {
                HarnessError::invalid_state(format!("could not encode harness catalog: {error}"))
            })
    }

    fn lock_repository(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HarnessRepository>, HarnessError> {
        self.repository
            .lock()
            .map_err(|_| HarnessError::invalid_state("harness lineage mutex is poisoned"))
    }
}

fn require_catalog_fields(
    object: &BTreeMap<String, JsonValue>,
    fields: &[&str],
) -> Result<(), HarnessLineageError> {
    for field in fields {
        if !object.contains_key(*field) {
            return Err(HarnessLineageError::Invalid {
                message: format!("harness catalog is missing required field {field}",),
            });
        }
    }
    for field in object.keys() {
        if !fields.contains(&field.as_str()) {
            return Err(HarnessLineageError::Invalid {
                message: format!("harness catalog has unknown field {field}",),
            });
        }
    }
    Ok(())
}

fn decode_harness_catalog(
    fact: &HarnessCatalogFact,
    artifacts: Arc<dyn ArtifactStore>,
    extension_engine: Arc<dyn ExtensionEngine>,
) -> Result<DecodedHarnessCatalog, HarnessLineageError> {
    if fact.schema_version != HARNESS_CATALOG_SCHEMA_VERSION {
        return Err(HarnessLineageError::Invalid {
            message: format!(
                "unsupported harness catalog schema version {}; expected {HARNESS_CATALOG_SCHEMA_VERSION}",
                fact.schema_version
            ),
        });
    }
    let bytes = artifacts
        .get(fact.artifact_id)
        .map_err(|error| HarnessLineageError::Artifact {
            message: format!(
                "required harness catalog artifact {} is unavailable: {error}",
                fact.artifact_id
            ),
        })?;
    if bytes.len() as u64 != fact.byte_len || ArtifactId::from_bytes(&bytes) != fact.artifact_id {
        return Err(HarnessLineageError::Invalid {
            message: "harness catalog artifact does not match its durable descriptor".into(),
        });
    }
    let source = std::str::from_utf8(&bytes).map_err(|_| HarnessLineageError::Invalid {
        message: "harness catalog artifact is not valid UTF-8 JSON".into(),
    })?;
    let value = JsonValue::parse(source).map_err(|error| HarnessLineageError::Invalid {
        message: format!("harness catalog artifact is invalid JSON: {error}"),
    })?;
    let canonical = value
        .to_json_string()
        .map_err(|error| HarnessLineageError::Invalid {
            message: format!("harness catalog artifact cannot encode canonically: {error}"),
        })?;
    if canonical.as_bytes() != bytes {
        return Err(HarnessLineageError::Invalid {
            message: "harness catalog artifact is not canonical JSON".into(),
        });
    }
    let object = value
        .as_object()
        .ok_or_else(|| HarnessLineageError::Invalid {
            message: "harness catalog root must be a JSON object".into(),
        })?;
    require_catalog_fields(
        object,
        &["schema_version", "capability_ceiling", "repository"],
    )?;
    let schema_version = object
        .get("schema_version")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| HarnessLineageError::Invalid {
            message: "harness catalog schema_version must be an unsigned integer".into(),
        })?;
    if schema_version != u64::from(HARNESS_CATALOG_SCHEMA_VERSION) {
        return Err(HarnessLineageError::Invalid {
            message: format!("unsupported harness catalog payload version {schema_version}"),
        });
    }
    let capability_ceiling = object
        .get("capability_ceiling")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| HarnessLineageError::Invalid {
            message: "harness catalog capability_ceiling must be an array".into(),
        })?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| HarnessLineageError::Invalid {
                    message: "harness catalog capability_ceiling must contain only strings".into(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut sorted_ceiling = capability_ceiling.clone();
    sorted_ceiling.sort();
    sorted_ceiling.dedup();
    if capability_ceiling != sorted_ceiling {
        return Err(HarnessLineageError::Invalid {
            message:
                "harness catalog capability_ceiling must be strictly sorted without duplicates"
                    .into(),
        });
    }
    let repository = HarnessRepository::from_catalog_json_with_extension_engine(
        artifacts,
        object
            .get("repository")
            .expect("required catalog field was checked"),
        extension_engine,
    )?;
    Ok(DecodedHarnessCatalog {
        capability_ceiling: capability_ceiling.into_iter().collect(),
        repository,
    })
}

fn resolve_snapshot(input: ResolveSnapshotInput<'_>) -> Result<ResolvedHarness, HarnessError> {
    let runtime_policies = input.runtime_services.runtime_policy_identities();
    let spec = &input.snapshot.spec;
    for (label, expected, actual) in [
        (
            "hook bundle",
            runtime_hook_bundle_digest(runtime_policies.hook_bundle_digest, spec),
            spec.hook_bundle_digest,
        ),
        (
            "automatic compaction",
            runtime_policies.compaction_policy_digest,
            spec.compaction_policy_digest,
        ),
        (
            "tool-result projection",
            runtime_policies.tool_projection_digest,
            spec.tool_projection_digest,
        ),
        (
            "tool-failure",
            runtime_policies.failure_policy_digest,
            spec.failure_policy_digest,
        ),
    ] {
        if expected != actual {
            return Err(HarnessError::invalid_state(format!(
                "resolved harness {label} identity does not match RuntimeServices (expected {}, actual {})",
                expected.to_hex(),
                actual.to_hex(),
            )));
        }
    }
    for name in input.plugin_tools.names() {
        if input.runtime_services.trusted_tools().get(name).is_some() {
            return Err(HarnessError::invalid_state(format!(
                "source-pinned plugin tool {name} collides with a trusted host capability",
            )));
        }
    }
    Ok(ResolvedHarness {
        identity: HarnessIdentity::new(
            input.revision.revision_id.clone(),
            input.snapshot.id.clone(),
            input.snapshot.spec.model_harness_profile.clone(),
        ),
        system_prompt: compose_system_prompt(&input.snapshot.spec),
        extension_tools: input.plugin_tools,
        host_commands: input.host_commands,
        idle_hooks: input.idle_hooks,
        hooks: input.hooks,
        automatic_compaction: input.runtime_services.automatic_compaction_policy().clone(),
        tool_result_projection: input
            .runtime_services
            .tool_result_projection_policy()
            .clone(),
        tool_failure_circuit_breaker: input.runtime_services.tool_failure_circuit_breaker_policy(),
        replay_safe_tools: input.runtime_services.replay_safe_tools().clone(),
        artifact_policy: input.runtime_services.artifact_policy_config().clone(),
        self_extension_mode: input.self_extension_mode,
        lifecycle: input.lifecycle,
        memory_collector: input.memory_collector,
        harness_snapshot: Some(input.snapshot.clone()),
        context_policies: input.context_policies,
    })
}

struct ResolveSnapshotInput<'a> {
    runtime_services: &'a RuntimeServices,
    revision: &'a HarnessRevisionV1,
    snapshot: &'a HarnessSnapshotV1,
    self_extension_mode: SelfExtensionMode,
    hooks: Arc<dyn tea_core::hooks::HookSet>,
    plugin_tools: ToolRegistry,
    host_commands: Vec<ResolvedHostCommand>,
    idle_hooks: Vec<ResolvedIdleHook>,
    lifecycle: PluginLifecycleRegistry,
    memory_collector: Arc<ExtensionMemoryCollector>,
    context_policies: ContextPolicyRegistry,
}

fn lineage_error(error: HarnessLineageError) -> HarnessError {
    HarnessError::invalid_state(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::extension::{
        ExtensionCapabilityBindings, ExtensionCommandInput, ExtensionCommandResult,
        ExtensionEngine, ExtensionError, ExtensionHostCommand, ExtensionHostCommandDescription,
        ExtensionLimits, ExtensionSourceTree, ResolvedExtension,
    };
    use crate::harness::{
        HarnessSeedBuilder, HarnessSeedExtension, HarnessSeedExtensionScope, ModelHarnessProfile,
    };
    use crate::scheduler::{
        CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream,
    };
    use crate::tool::ToolRegistry;

    #[derive(Debug)]
    struct UnusedProvider;

    impl ModelProvider for UnusedProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            Box::pin(std::future::ready(
                Ok(Box::new(ModelStream::default()) as _),
            ))
        }
    }

    #[derive(Clone)]
    struct FixtureCommand {
        description: ExtensionHostCommandDescription,
    }

    impl ExtensionHostCommand for FixtureCommand {
        fn description(&self) -> &ExtensionHostCommandDescription {
            &self.description
        }

        fn invoke(
            &self,
            _input: &ExtensionCommandInput,
        ) -> Result<ExtensionCommandResult, ExtensionError> {
            Ok(ExtensionCommandResult::default())
        }
    }

    #[derive(Clone, Copy)]
    struct CommandExtensionEngine;

    impl ExtensionEngine for CommandExtensionEngine {
        fn describe(
            &self,
            _source: &ExtensionSourceTree,
        ) -> Result<crate::harness::extension::ExtensionDescriptor, ExtensionError> {
            Ok(crate::harness::extension::ExtensionDescriptor {
                requested_capabilities: BTreeSet::new(),
                prompt_sections: Vec::new(),
                tools: Vec::new(),
                host_commands: vec![ExtensionHostCommandDescription {
                    name: "/native".into(),
                    help: "fixture command".into(),
                    allowed_while_active: false,
                }],
                lifecycle_hook_ids: Vec::new(),
            })
        }

        fn resolve(
            &self,
            _source: &ExtensionSourceTree,
            _bindings: ExtensionCapabilityBindings,
            inner_hooks: Arc<dyn HookSet>,
            _extension_index: usize,
            _memory_collector: Arc<ExtensionMemoryCollector>,
        ) -> Result<ResolvedExtension, ExtensionError> {
            Ok(ResolvedExtension {
                hooks: inner_hooks,
                tools: ToolRegistry::default(),
                host_commands: vec![Arc::new(FixtureCommand {
                    description: ExtensionHostCommandDescription {
                        name: "/native".into(),
                        help: "fixture command".into(),
                        allowed_while_active: false,
                    },
                })],
                idle_hook: None,
                context_policy: None,
                lifecycle: None,
            })
        }
    }

    #[test]
    fn native_host_command_collisions_fail_extension_resolution() {
        let artifacts = Arc::new(tea_session::MemoryArtifactStore::default());
        let services = RuntimeServices::new(Arc::new(UnusedProvider), ToolRegistry::default());
        let profile = ModelHarnessProfile::new(
            "fixture",
            "fixture-model",
            None,
            "fixture-prompt",
            "fixture-tools",
            "fixture-compaction",
            "fixture-projection",
        )
        .expect("fixture profile is valid");
        let limits = HarnessTreeLimits::default();
        let resource_limits = crate::harness::HarnessResourceLimits::default();
        let source = ExtensionSourceTree {
            extension_id: "fixture.extension".into(),
            files: BTreeMap::from([("entry.luau".into(), "return {}".into())]),
            expected_capabilities: Some(BTreeSet::new()),
            limits: ExtensionLimits {
                max_source_bytes: resource_limits.source_bytes,
                max_memory_bytes: resource_limits.memory_bytes,
                max_interrupt_checks: resource_limits.instruction_checks as usize,
            },
        };
        let seeded = HarnessSeedBuilder::new(
            artifacts,
            Arc::new(CommandExtensionEngine),
            tea_session::Digest::from_bytes("fixture-host-profile"),
            "fixture system prompt",
            profile,
            SelfExtensionMode::Off,
            resource_limits,
            services.runtime_policy_identities(),
        )
        .tree_limits(limits)
        .extensions(vec![HarnessSeedExtension {
            scope: HarnessSeedExtensionScope::Global,
            source,
        }])
        .seed(HarnessActor::Host, 1)
        .expect("fixture harness seeds");
        let manager = HarnessResolver::new(seeded.repository, BTreeSet::new())
            .reserved_extension_command_names(["/native"]);

        let error = manager
            .resolve_revision(&seeded.revision.revision_id, &services)
            .expect_err("native command collision must fail resolution");
        assert!(
            error
                .to_string()
                .contains("collides with a native host command")
        );
    }
}

#[derive(Clone)]
struct MutableSourceFile {
    artifact_id: ArtifactId,
    bytes: Vec<u8>,
    media_type: String,
}

fn collect_session_source_files(
    repository: &HarnessRepository,
    snapshot: &HarnessSnapshotV1,
) -> Result<BTreeMap<NormalizedPath, MutableSourceFile>, HarnessError> {
    let mut files = BTreeMap::<NormalizedPath, MutableSourceFile>::new();
    for plugin in &snapshot.spec.ordered_session_plugins {
        for source in repository
            .tree_source_files(&plugin.tree_id)
            .map_err(lineage_error)?
        {
            let Some(plugin_id) = plugin_id_from_path(&source.path) else {
                continue;
            };
            if plugin_id != plugin.plugin_id {
                continue;
            }
            let next = MutableSourceFile {
                artifact_id: source.artifact_id,
                bytes: source.bytes,
                media_type: source.media_type,
            };
            match files.get(&source.path) {
                Some(existing)
                    if existing.artifact_id == next.artifact_id
                        && existing.media_type == next.media_type => {}
                Some(_) => {
                    return Err(HarnessError::invalid_state(format!(
                        "session plugin trees disagree about immutable source {}",
                        source.path
                    )));
                }
                None => {
                    files.insert(source.path, next);
                }
            }
        }
    }
    Ok(files)
}

fn snapshot_for_revision<'a>(
    repository: &'a HarnessRepository,
    revision_id: &HarnessRevisionId,
) -> Result<&'a HarnessSnapshotV1, HarnessError> {
    let revision = repository.revision(revision_id).ok_or_else(|| {
        HarnessError::invalid_state(format!("unknown harness revision {revision_id}"))
    })?;
    repository.snapshot(&revision.snapshot_id).ok_or_else(|| {
        HarnessError::invalid_state(format!(
            "revision {revision_id} references missing snapshot {}",
            revision.snapshot_id,
        ))
    })
}

fn source_files_for_snapshot(
    repository: &HarnessRepository,
    snapshot: &HarnessSnapshotV1,
) -> Result<BTreeMap<NormalizedPath, HarnessSourceFile>, HarnessError> {
    let mut files = BTreeMap::<NormalizedPath, HarnessSourceFile>::new();
    for plugin in snapshot
        .spec
        .ordered_global_plugins
        .iter()
        .chain(snapshot.spec.ordered_session_plugins.iter())
    {
        let prefix = format!("plugins/{}/", plugin.plugin_id);
        for source in repository
            .tree_source_files(&plugin.tree_id)
            .map_err(lineage_error)?
            .into_iter()
            .filter(|source| source.path.as_str().starts_with(&prefix))
        {
            match files.get(&source.path) {
                Some(existing) if existing.artifact_id == source.artifact_id => {}
                Some(_) => {
                    return Err(HarnessError::invalid_state(format!(
                        "immutable snapshot has conflicting source identities for {}",
                        source.path,
                    )));
                }
                None => {
                    files.insert(source.path.clone(), source);
                }
            }
        }
    }
    Ok(files)
}

fn revision_is_ancestor(
    repository: &HarnessRepository,
    ancestor: &HarnessRevisionId,
    descendant: &HarnessRevisionId,
) -> Result<bool, HarnessError> {
    let mut pending = vec![descendant.clone()];
    let mut visited = BTreeSet::new();
    while let Some(current) = pending.pop() {
        if !visited.insert(current.clone()) {
            continue;
        }
        if &current == ancestor {
            return Ok(true);
        }
        let revision = repository.revision(&current).ok_or_else(|| {
            HarnessError::invalid_state(format!("unknown harness revision {current}"))
        })?;
        pending.extend(revision.parent_revision_ids.iter().cloned());
    }
    Ok(false)
}

fn apply_source_patches(
    files: &mut BTreeMap<NormalizedPath, MutableSourceFile>,
    patches: &[HarnessFilePatch],
    current_plugin_ids: &BTreeSet<String>,
    added_plugin_ids: &BTreeSet<String>,
    global_plugin_ids: &BTreeSet<String>,
) -> Result<(), HarnessError> {
    let mut touched = BTreeSet::new();
    for patch in patches {
        let path = match patch {
            HarnessFilePatch::Upsert { path, .. } | HarnessFilePatch::Delete { path, .. } => path,
        };
        if !touched.insert(path.clone()) {
            return Err(HarnessError::invalid_state(format!(
                "harness apply repeats source path {path}",
            )));
        }
        let plugin_id = plugin_id_from_path(path).ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "harness source path {path} must live below plugins/<plugin-id>/",
            ))
        })?;
        if global_plugin_ids.contains(plugin_id) {
            return Err(HarnessError::invalid_state(format!(
                "harness apply cannot modify operator-pinned global plugin {plugin_id}",
            )));
        }
        if !current_plugin_ids.contains(plugin_id) && !added_plugin_ids.contains(plugin_id) {
            return Err(HarnessError::invalid_state(format!(
                "harness apply path {path} is not owned by an existing or added session plugin",
            )));
        }
        match patch {
            HarnessFilePatch::Upsert { content, .. } => {
                files.insert(
                    path.clone(),
                    MutableSourceFile {
                        artifact_id: ArtifactId::from_bytes(content.as_bytes()),
                        bytes: content.as_bytes().to_vec(),
                        media_type: source_media_type(path)?,
                    },
                );
            }
            HarnessFilePatch::Delete {
                expected_artifact_id,
                ..
            } => {
                let existing = files.get(path).ok_or_else(|| {
                    HarnessError::invalid_state(format!(
                        "harness apply cannot delete missing source {path}",
                    ))
                })?;
                if existing.artifact_id != *expected_artifact_id {
                    return Err(HarnessError::invalid_state(format!(
                        "harness apply delete for {path} has a stale expected artifact digest",
                    )));
                }
                files.remove(path);
            }
        }
    }
    Ok(())
}

fn apply_registry_operations(
    snapshot: &HarnessSnapshotV1,
    operations: &[RegistryOperation],
) -> Result<Vec<String>, HarnessError> {
    let mut ordered = snapshot
        .spec
        .ordered_session_plugins
        .iter()
        .map(|plugin| plugin.plugin_id.clone())
        .collect::<Vec<_>>();
    for operation in operations {
        match operation {
            RegistryOperation::Add { plugin_id } => {
                validate_plugin_id(plugin_id)?;
                if ordered.iter().any(|existing| existing == plugin_id) {
                    return Err(HarnessError::invalid_state(format!(
                        "harness registry already contains plugin {plugin_id}",
                    )));
                }
                ordered.push(plugin_id.clone());
            }
            RegistryOperation::Remove { plugin_id } => {
                validate_plugin_id(plugin_id)?;
                let before = ordered.len();
                ordered.retain(|existing| existing != plugin_id);
                if ordered.len() == before {
                    return Err(HarnessError::invalid_state(format!(
                        "harness registry cannot remove unknown session plugin {plugin_id}",
                    )));
                }
            }
        }
    }
    Ok(ordered)
}

fn changed_surfaces(
    parent: &HarnessSnapshotV1,
    proposed: &HarnessSnapshotV1,
) -> BTreeSet<HarnessSurface> {
    let mut changed = BTreeSet::new();
    if parent.fingerprints.system_prompt_digest != proposed.fingerprints.system_prompt_digest {
        changed.insert(HarnessSurface::SystemPrompt);
    }
    if parent.fingerprints.ordered_tool_definitions_digest
        != proposed.fingerprints.ordered_tool_definitions_digest
    {
        changed.insert(HarnessSurface::ToolDefinitions);
    }
    if parent.fingerprints.tool_execution_policy_digest
        != proposed.fingerprints.tool_execution_policy_digest
    {
        changed.insert(HarnessSurface::ToolExecutionPolicy);
    }
    if parent.fingerprints.hook_bundle_digest != proposed.fingerprints.hook_bundle_digest {
        changed.insert(HarnessSurface::Hooks);
    }
    if parent.fingerprints.capability_bindings_digest
        != proposed.fingerprints.capability_bindings_digest
    {
        changed.insert(HarnessSurface::CapabilityBindings);
    }
    if parent.spec.compaction_policy_digest != proposed.spec.compaction_policy_digest {
        changed.insert(HarnessSurface::Compaction);
    }
    if parent.spec.tool_projection_digest != proposed.spec.tool_projection_digest {
        changed.insert(HarnessSurface::ToolProjection);
    }
    if parent.spec.failure_policy_digest != proposed.spec.failure_policy_digest {
        changed.insert(HarnessSurface::FailurePolicy);
    }
    changed
}

fn source_media_type(path: &NormalizedPath) -> Result<String, HarnessError> {
    if path.as_str().ends_with("/manifest.json") {
        Ok("application/json".into())
    } else if path.as_str().ends_with(".luau") {
        Ok("text/plain".into())
    } else {
        Err(HarnessError::invalid_state(format!(
            "harness source path {path} must be manifest.json or a declared .luau module",
        )))
    }
}

fn plugin_id_from_path(path: &NormalizedPath) -> Option<&str> {
    path.as_str()
        .strip_prefix("plugins/")?
        .split_once('/')
        .map(|(plugin_id, _)| plugin_id)
        .filter(|plugin_id| !plugin_id.is_empty())
}

fn validate_plugin_id(value: &str) -> Result<(), HarnessError> {
    if value.is_empty()
        || value.len() > 120
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    {
        return Err(HarnessError::invalid_state(
            "plugin IDs must use the portable [A-Za-z0-9._-] spelling",
        ));
    }
    Ok(())
}
