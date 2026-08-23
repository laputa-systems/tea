//! Immutable harness-revision resolution and activation validation.
//!
//! The manager is deliberately narrower than the durable supervisor.  It
//! owns immutable lineage objects and turns a committed revision into a fresh
//! executable epoch template, but it has no session writer and cannot change
//! a lane by itself.  The supervisor remains responsible for writing a
//! `HarnessRevisionChanged` semantic entry at an epoch boundary.

use crate::lineage::compose_system_prompt;
use crate::context::ContextPolicyRegistry;
use crate::lifecycle::PluginLifecycleRegistry;
use crate::{
    CandidateHypothesis, CoreEpochTemplate, HarnessActor, HarnessCandidateDraft,
    HarnessCandidateV1, HarnessError, HarnessIdentity, HarnessLineageError, HarnessRepository,
    HarnessRevisionV1, HarnessSnapshotV1, HarnessSourceFile, HarnessSurface, HarnessTreeLimits, PluginBundleRef,
    PluginCapabilityCatalog, RegistryOperation, SelfExtensionMode,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use tea_core::tool::ToolRegistry;
use tea_luau::tool_handler::{LuaToolHandler, ToolHandlerSpec};
use tea_luau::{LuaPolicyHookSet, PolicyMemoryCollector};
use tea_session::{
    ArtifactId, ArtifactStore, CanonicalHashWriter, HarnessCandidateId, HarnessCatalogFact,
    HarnessRevisionId, NormalizedPath, OperationId, SessionFact, SessionWriter,
};
use tea_protocol::JsonValue;

/// Fixed artifact media type for immutable harness repository catalogs.
pub(crate) const HARNESS_CATALOG_MEDIA_TYPE: &str = "application/vnd.tea.harness-catalog+json";
const HARNESS_CATALOG_SCHEMA_VERSION: u16 = 1;

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
pub struct ResolvedHarnessConfiguration {
    /// Exact immutable attribution for the epoch.
    pub identity: HarnessIdentity,
    /// Fresh executable core configuration for the selected snapshot.
    pub template: CoreEpochTemplate,
    /// Frozen session-level self-extension exposure for this epoch.
    pub self_extension_mode: SelfExtensionMode,
    /// Fresh source-pinned, capability-free lifecycle policies for this
    /// immutable snapshot. The field stays crate-private because only the
    /// durable supervisor may consume or persist their output.
    pub(crate) lifecycle: PluginLifecycleRegistry,
    /// Process-local post-tool memory proposals emitted by the exact policy
    /// VMs used for this epoch. The supervisor consumes these only after raw
    /// tool evidence has committed.
    pub(crate) memory_collector: Arc<PolicyMemoryCollector>,
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

impl std::fmt::Debug for ResolvedHarnessConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedHarnessConfiguration")
            .field("identity", &self.identity)
            .field("self_extension_mode", &self.self_extension_mode)
            .finish_non_exhaustive()
    }
}

/// Session-local immutable harness catalog.
///
/// A host initializes it from the globally pinned source inputs that belong to
/// one session.  It can stage and validate candidates, but activation returns
/// an immutable revision only; a caller must still commit that revision to the
/// session branch through the durable activation protocol.
pub struct HarnessManager {
    repository: Mutex<HarnessRepository>,
    base_template: CoreEpochTemplate,
    capability_ceiling: BTreeSet<String>,
    capability_catalog: PluginCapabilityCatalog,
    tree_limits: HarnessTreeLimits,
    self_extension_mode: SelfExtensionMode,
}

impl std::fmt::Debug for HarnessManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessManager")
            .field("capability_ceiling", &self.capability_ceiling)
            .field("capability_catalog", &self.capability_catalog)
            .field("tree_limits", &self.tree_limits)
            .field("self_extension_mode", &self.self_extension_mode)
            .finish_non_exhaustive()
    }
}

impl HarnessManager {
    /// Create a session-local catalog from immutable source/snapshot lineage
    /// and the trusted base executable capabilities.
    pub fn new(
        repository: HarnessRepository,
        base_template: CoreEpochTemplate,
        capability_ceiling: BTreeSet<String>,
    ) -> Self {
        Self {
            repository: Mutex::new(repository),
            base_template,
            capability_ceiling,
            capability_catalog: PluginCapabilityCatalog::default(),
            tree_limits: HarnessTreeLimits::default(),
            self_extension_mode: SelfExtensionMode::Off,
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
    ) -> Result<ResolvedHarnessConfiguration, HarnessError> {
        let (revision, snapshot, policies) = {
            let repository = self.lock_repository()?;
            let revision = repository
                .revision(revision_id)
                .cloned()
                .ok_or_else(|| HarnessError::invalid_state(format!(
                    "unknown harness revision {revision_id}"
                )))?;
            let snapshot = repository
                .snapshot(&revision.snapshot_id)
                .cloned()
                .ok_or_else(|| HarnessError::invalid_state(format!(
                    "revision {revision_id} references missing snapshot {}",
                    revision.snapshot_id
                )))?;
            let policies = repository
                .load_plugin_policies(&snapshot)
                .map_err(lineage_error)?;
            (revision, snapshot, policies)
        };
        let mut resolved_bindings = BTreeMap::new();
        let lifecycle = PluginLifecycleRegistry::from_loaded(&policies)?;
        let context_policies = ContextPolicyRegistry::from_loaded(&policies);
        for loaded in &policies {
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
        let mut plugin_tools = ToolRegistry::default();
        for loaded in &policies {
            for tool in loaded.policy.tools() {
                let handler_source = tool.handler_source.as_ref().ok_or_else(|| {
                    HarnessError::invalid_state(format!(
                        "plugin {} declares model-visible tool {} without a bounded handler_source",
                        loaded.plugin.plugin_id, tool.name,
                    ))
                })?;
                let binding = resolved_bindings
                    .get(&(loaded.plugin.plugin_id.clone(), tool.capability.clone()))
                    .ok_or_else(|| HarnessError::invalid_state(format!(
                        "plugin {} tool {} requests unbound capability {}",
                        loaded.plugin.plugin_id, tool.name, tool.capability,
                    )))?;
                if plugin_tools.get(&tool.name).is_some() {
                    return Err(HarnessError::invalid_state(format!(
                        "plugins declare duplicate executable tool {}",
                        tool.name,
                    )));
                }
                let handler = LuaToolHandler::new_with_limits(
                    handler_source.clone(),
                    ToolHandlerSpec {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        schema: tool.schema.clone(),
                        capability: tool.capability.clone(),
                        execution_mode: tool.execution_mode,
                    },
                    binding.capabilities.clone(),
                    binding.handler_limits,
                )
                .map_err(|error| HarnessError::invalid_state(format!(
                    "plugin {} tool {} could not bind its validated handler: {error}",
                    loaded.plugin.plugin_id, tool.name,
                )))?;
                plugin_tools.insert(Arc::new(handler));
            }
        }
        let memory_collector = Arc::new(PolicyMemoryCollector::default());
        let mut hooks = self.base_template.hook_set();
        for (index, loaded) in policies.iter().enumerate().rev() {
            hooks = Arc::new(LuaPolicyHookSet::new_with_memory(
                Arc::clone(&loaded.policy),
                loaded.plugin.plugin_id.clone(),
                index,
                Arc::clone(&memory_collector),
                hooks,
            ));
        }
        resolve_snapshot(
            &self.base_template,
            &revision,
            &snapshot,
            self.self_extension_mode,
            hooks,
            plugin_tools,
            lifecycle,
            memory_collector,
            context_policies,
        )
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
    pub fn apply(&self, request: HarnessApplyRequest) -> Result<HarnessCandidateV1, HarnessError> {
        if request.tool_invocation_id.trim().is_empty() {
            return Err(HarnessError::invalid_state(
                "harness apply requires a stable tool invocation identity",
            ));
        }
        let mut repository = self.lock_repository()?;
        let parent_revision = repository
            .revision(&request.base_revision_id)
            .cloned()
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "harness apply base revision {} does not exist",
                request.base_revision_id
            )))?;
        let parent_snapshot = repository
            .snapshot(&parent_revision.snapshot_id)
            .cloned()
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "harness apply base revision {} has no immutable snapshot",
                request.base_revision_id
            )))?;
        let target_plugin_ids = apply_registry_operations(
            &parent_snapshot,
            &request.registry_operations,
        )?;
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
                            .plugin_capabilities(&tree.id, plugin_id)
                            .map_err(lineage_error)?,
                    })
                })
                .collect::<Result<Vec<_>, HarnessError>>()?
        };
        proposed_spec.hook_bundle_digest = session_plugin_hook_digest(&proposed_spec);
        let proposed_snapshot = repository
            .stage_snapshot(proposed_spec)
            .map_err(lineage_error)?;
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
                    HarnessFilePatch::Upsert { path, .. } | HarnessFilePatch::Delete { path, .. } => {
                        path.clone()
                    }
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
            .ok_or_else(|| HarnessError::invalid_state(format!("unknown harness candidate {candidate_id}")))
    }

    /// Return a cloned immutable revision for status or durable activation.
    pub fn revision(
        &self,
        revision_id: &HarnessRevisionId,
    ) -> Result<HarnessRevisionV1, HarnessError> {
        self.lock_repository()?
            .revision(revision_id)
            .cloned()
            .ok_or_else(|| HarnessError::invalid_state(format!("unknown harness revision {revision_id}")))
    }

    /// Return a cloned snapshot for read-only status and diffing.
    pub fn snapshot_for_revision(
        &self,
        revision_id: &HarnessRevisionId,
    ) -> Result<HarnessSnapshotV1, HarnessError> {
        let repository = self.lock_repository()?;
        let revision = repository
            .revision(revision_id)
            .ok_or_else(|| HarnessError::invalid_state(format!("unknown harness revision {revision_id}")))?;
        repository
            .snapshot(&revision.snapshot_id)
            .cloned()
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "revision {revision_id} references missing snapshot {}",
                revision.snapshot_id
            )))
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
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "unknown harness snapshot {snapshot_id}",
            )))
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
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "revision {revision_id} does not contain immutable source {path}",
            )))
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
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "harness rollback base revision {base_revision_id} does not exist",
            )))?;
        let target = repository
            .revision(&target_revision_id)
            .cloned()
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "harness rollback target revision {target_revision_id} does not exist",
            )))?;
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
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "harness rollback base revision {base_revision_id} has no immutable snapshot",
            )))?;
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
            .map_err(|error| HarnessError::invalid_state(format!(
                "could not persist immutable harness catalog: {error}"
            )))?;
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
        if fact.schema_version != HARNESS_CATALOG_SCHEMA_VERSION {
            return Err(HarnessError::invalid_state(format!(
                "unsupported harness catalog schema version {}; expected {HARNESS_CATALOG_SCHEMA_VERSION}",
                fact.schema_version
            )));
        }
        let bytes = artifacts
            .get(fact.artifact_id)
            .map_err(|error| HarnessError::invalid_state(format!(
                "required harness catalog artifact {} is unavailable: {error}",
                fact.artifact_id
            )))?;
        if bytes.len() as u64 != fact.byte_len || ArtifactId::from_bytes(&bytes) != fact.artifact_id {
            return Err(HarnessError::invalid_state(
                "harness catalog artifact does not match its durable descriptor",
            ));
        }
        let value = JsonValue::parse(
            std::str::from_utf8(&bytes).map_err(|_| {
                HarnessError::invalid_state("harness catalog artifact is not valid UTF-8 JSON")
            })?,
        )
        .map_err(|error| HarnessError::invalid_state(format!(
            "harness catalog artifact is invalid JSON: {error}"
        )))?;
        let object = value.as_object().ok_or_else(|| {
            HarnessError::invalid_state("harness catalog root must be a JSON object")
        })?;
        require_catalog_fields(object, &["schema_version", "capability_ceiling", "repository"])?;
        let schema_version = object
            .get("schema_version")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| HarnessError::invalid_state("harness catalog schema_version must be an unsigned integer"))?;
        if schema_version != u64::from(HARNESS_CATALOG_SCHEMA_VERSION) {
            return Err(HarnessError::invalid_state(format!(
                "unsupported harness catalog payload version {schema_version}"
            )));
        }
        let ceiling = object
            .get("capability_ceiling")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| HarnessError::invalid_state("harness catalog capability_ceiling must be an array"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| HarnessError::invalid_state(
                        "harness catalog capability_ceiling must contain only strings",
                    ))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if ceiling != self.capability_ceiling {
            return Err(HarnessError::invalid_state(
                "durable harness catalog capability ceiling does not match the trusted session manager configuration",
            ));
        }
        let repository = HarnessRepository::from_catalog_json(
            artifacts,
            object.get("repository").expect("required catalog field was checked"),
        )
        .map_err(lineage_error)?;
        *self.lock_repository()? = repository;
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
            ("repository", repository.catalog_json().map_err(lineage_error)?),
        ]);
        payload.to_json_string().map(|text| text.into_bytes()).map_err(|error| {
            HarnessError::invalid_state(format!("could not encode harness catalog: {error}"))
        })
    }

    fn lock_repository(&self) -> Result<std::sync::MutexGuard<'_, HarnessRepository>, HarnessError> {
        self.repository
            .lock()
            .map_err(|_| HarnessError::invalid_state("harness lineage mutex is poisoned"))
    }
}

fn require_catalog_fields(
    object: &BTreeMap<String, JsonValue>,
    fields: &[&str],
) -> Result<(), HarnessError> {
    for field in fields {
        if !object.contains_key(*field) {
            return Err(HarnessError::invalid_state(format!(
                "harness catalog is missing required field {field}",
            )));
        }
    }
    for field in object.keys() {
        if !fields.contains(&field.as_str()) {
            return Err(HarnessError::invalid_state(format!(
                "harness catalog has unknown field {field}",
            )));
        }
    }
    Ok(())
}

fn resolve_snapshot(
    base_template: &CoreEpochTemplate,
    revision: &HarnessRevisionV1,
    snapshot: &HarnessSnapshotV1,
    self_extension_mode: SelfExtensionMode,
    hooks: Arc<dyn tea_core::hooks::HookSet>,
    plugin_tools: ToolRegistry,
    lifecycle: PluginLifecycleRegistry,
    memory_collector: Arc<PolicyMemoryCollector>,
    context_policies: ContextPolicyRegistry,
) -> Result<ResolvedHarnessConfiguration, HarnessError> {
    Ok(ResolvedHarnessConfiguration {
        identity: HarnessIdentity::new(
            revision.revision_id.clone(),
            snapshot.id.clone(),
            snapshot.spec.model_harness_profile.clone(),
        ),
        template: base_template.with_resolved_plugins(
            compose_system_prompt(&snapshot.spec),
            hooks,
            plugin_tools,
        )?,
        self_extension_mode,
        lifecycle,
        memory_collector,
        harness_snapshot: Some(snapshot.clone()),
        context_policies,
    })
}

fn lineage_error(error: HarnessLineageError) -> HarnessError {
    HarnessError::invalid_state(error.to_string())
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
                    )))
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
    let revision = repository
        .revision(revision_id)
        .ok_or_else(|| HarnessError::invalid_state(format!("unknown harness revision {revision_id}")))?;
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
                    )))
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

/// Canonical identity of the ordered session-plugin hook contribution.
///
/// Hosts use this when seeding a snapshot that already contains session
/// plugins, and candidate application uses the same calculation so an exact
/// source reapplication remains a detectable no-op.
pub(crate) fn session_plugin_hook_digest(
    spec: &crate::HarnessSnapshotSpec,
) -> tea_session::Digest {
    let mut writer = CanonicalHashWriter::new("tea-session-plugin-hooks-v1", 1, 2);
    writer.u64(
        "plugin_count",
        spec.ordered_session_plugins.len() as u64,
    );
    for plugin in &spec.ordered_session_plugins {
        writer.string("plugin_id", &plugin.plugin_id);
        writer.string("tree_id", plugin.tree_id.as_str());
    }
    writer.finish()
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
        || value.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
    {
        return Err(HarnessError::invalid_state(
            "plugin IDs must use the portable [A-Za-z0-9._-] spelling",
        ));
    }
    Ok(())
}
