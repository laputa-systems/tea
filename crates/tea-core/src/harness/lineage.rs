//! Immutable harness source trees, snapshots, revisions, and staged candidates.
//!
//! This module intentionally owns lineage metadata rather than executable
//! provider or tool state. A running core epoch receives one resolved
//! `RuntimeServices`; edits always produce a candidate here first and can
//! become active only through a later durable revision transition.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use tea_core::harness::extension::{ExtensionEngine, ExtensionLimits, ExtensionSourceTree};
use tea_protocol::JsonValue;
use tea_session::{
    ArtifactId, ArtifactStore, CanonicalHashWriter, Digest, HarnessCandidateId, HarnessRevisionId,
    HarnessSnapshotId, HarnessTreeId, ModelHarnessProfileId, NormalizedPath, OperationId,
};

mod catalog;

const SNAPSHOT_SCHEMA_VERSION: u16 = 1;
const EXTENSION_ABI_VERSION: u16 = 1;

/// Actor that created a revision or candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HarnessActor {
    /// Trusted Rust-owned profile initialization.
    Host,
    /// An operator-approved global change.
    Operator,
    /// A model-proposed session-local candidate.
    Model,
}

/// Durable reason for an immutable revision transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessRevisionReason {
    /// Initial session pinning.
    Initial,
    /// A validated session-local candidate became active.
    CandidateActivation,
    /// An operator requested an immutable rebase of global inputs.
    GlobalRebase,
    /// A revision selected an earlier immutable snapshot.
    Rollback,
}

/// A provider-visible or host-only surface affected by a candidate.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum HarnessSurface {
    /// Stable system instructions.
    SystemPrompt,
    /// Ordered model-visible tool definitions.
    ToolDefinitions,
    /// Hook code or hook descriptors.
    Hooks,
    /// Bound host capability identities.
    CapabilityBindings,
    /// Compaction policy or descriptor.
    Compaction,
    /// Tool-result projection policy or descriptor.
    ToolProjection,
    /// Host failure policy.
    FailurePolicy,
    /// Host-only tool execution and cancellation policy.
    ToolExecutionPolicy,
}

/// Explicit resource ceilings frozen in an immutable snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessResourceLimits {
    /// Maximum source bytes in one plugin bundle.
    pub source_bytes: usize,
    /// Maximum Luau heap bytes for one policy invocation.
    pub memory_bytes: usize,
    /// Cooperative interrupt checks allowed for one policy invocation.
    pub instruction_checks: u32,
    /// Maximum provider-visible prompt bytes contributed by the harness.
    pub provider_surface_bytes: usize,
}

impl Default for HarnessResourceLimits {
    fn default() -> Self {
        Self {
            source_bytes: 65_536,
            // The web policy renders a bounded multi-source response and is
            // still isolated per invocation; this ceiling accommodates its
            // compiled Luau handler without weakening source or instruction
            // bounds.
            memory_bytes: 1536 * 1024,
            // A web result can carry several bounded UTF-8 source excerpts;
            // rendering that material is still subject to the heap ceiling,
            // but needs a larger cooperative instruction allowance than the
            // tiny control-oriented bundled policies.
            instruction_checks: 100_000,
            provider_surface_bytes: 256 * 1024,
        }
    }
}

/// Limits for one immutable source-tree staging operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessTreeLimits {
    /// Maximum source files in a staged tree.
    pub maximum_files: usize,
    /// Maximum total exact source bytes in a staged tree.
    pub maximum_source_bytes: usize,
}

impl Default for HarnessTreeLimits {
    fn default() -> Self {
        Self {
            maximum_files: 256,
            maximum_source_bytes: 1_048_576,
        }
    }
}

/// Immutable source blob metadata within a source tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessTreeFile {
    /// Canonical portable source path.
    pub path: NormalizedPath,
    /// Exact content-addressed source bytes.
    pub artifact_id: ArtifactId,
    /// Exact source byte length.
    pub byte_len: u64,
    /// Stable content type used to reload the source.
    pub media_type: String,
}

/// Exact source content materialized from an immutable tree for a candidate
/// patch.  The caller must stage a new tree after editing; this value never
/// provides a mutable handle to the original content-addressed object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessSourceFile {
    /// Canonical tree-relative path.
    pub path: NormalizedPath,
    /// Existing content-addressed object identity, useful for guarded delete.
    pub artifact_id: ArtifactId,
    /// Exact UTF-8 or binary source bytes.
    pub bytes: Vec<u8>,
    /// Stable content type retained with the source object.
    pub media_type: String,
}

/// One immutable content-addressed set of source files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessTree {
    /// Canonical tree identity derived from sorted file metadata.
    pub id: HarnessTreeId,
    /// Sorted immutable source metadata by canonical path.
    pub files: BTreeMap<NormalizedPath, HarnessTreeFile>,
}

/// A plugin bundle pinned by a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginBundleRef {
    /// Portable plugin identity.
    pub plugin_id: String,
    /// Tree containing the exact plugin source files.
    pub tree_id: HarnessTreeId,
    /// Requested capabilities frozen when the bundle was validated.
    pub requested_capabilities: BTreeSet<String>,
}

/// Exact extension source paired with the immutable bundle reference that
/// selected it. Executable extension state is rebuilt by the host-selected
/// [`ExtensionEngine`] for each harness resolution or recovery.
pub(crate) struct LoadedExtensionSource {
    pub(crate) plugin: PluginBundleRef,
    pub(crate) source: ExtensionSourceTree,
}

/// Deterministically ordered prompt contribution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptSectionDescriptor {
    /// Stable section identity used by validation and diagnostics.
    pub id: String,
    /// Exact provider-visible text.
    pub content: String,
}

/// One ordered model-facing tool presentation plus its host-only execution policy.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolPresentationDescriptor {
    /// Stable registered name.
    pub name: String,
    /// Exact model-facing description.
    pub description: String,
    /// Canonical provider-visible JSON schema.
    pub schema: JsonValue,
    /// Stable host-only execution-mode spelling (`sequential` or `parallel`).
    pub execution_mode: String,
    /// Whether the tool must be the sole call in an assistant batch.
    pub requires_exclusive_batch: bool,
    /// Host-only cancellation settlement spelling.
    pub cancellation_settlement_mode: String,
}

/// Exact capability binding selected by a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityBindingRef {
    /// Immutable plugin identity that may consume this grant. A capability
    /// spelling by itself is never a trust identity because two bundles may
    /// request the same narrow host operation under distinct revisions.
    pub plugin_id: String,
    /// Capability namespace/name approved by the host.
    pub capability: String,
    /// Host-selected capability ABI/version label. It participates in the
    /// immutable snapshot so an implementation upgrade cannot silently alter
    /// an existing session's executable authority.
    pub capability_version: String,
    /// Immutable host binding identity, not a raw secret or ambient handle.
    pub binding_digest: Digest,
}

/// Independent stable fingerprints for cache and policy diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessSurfaceFingerprints {
    /// Exact composed provider system prompt bytes.
    pub system_prompt_digest: Digest,
    /// Ordered provider-visible tool-definition bytes.
    pub ordered_tool_definitions_digest: Digest,
    /// Ordered host-only tool execution-policy bytes.
    pub tool_execution_policy_digest: Digest,
    /// Hook bundle/source descriptor identity.
    pub hook_bundle_digest: Digest,
    /// Bound host capability identities.
    pub capability_bindings_digest: Digest,
    /// Compaction descriptor identity.
    pub compaction_policy_digest: Digest,
    /// Exact provider-visible prompt plus ordered tools.
    pub provider_surface_digest: Digest,
}

/// Input needed to resolve one immutable snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessSnapshotSpec {
    /// Trusted immutable base-profile content identity.
    pub base_profile_digest: Digest,
    /// Trusted base prompt text, never editable by a candidate.
    pub base_system_prompt: String,
    /// Exact model harness profile.
    pub model_harness_profile: ModelHarnessProfileId,
    /// Optional stable extension addendum version/content.
    pub self_extension_addendum: Option<String>,
    /// Ordered operator-pinned global plugin bundles.
    pub ordered_global_plugins: Vec<PluginBundleRef>,
    /// Ordered session-local plugin bundles.
    pub ordered_session_plugins: Vec<PluginBundleRef>,
    /// Ordered prompt sections after the trusted prefix.
    pub prompt_sections: Vec<PromptSectionDescriptor>,
    /// Exact source-derived sections from the ordered immutable plugin
    /// registry.  [`HarnessRepository::stage_snapshot`] recomputes these from
    /// closed Luau bundles; callers cannot make source edits that leave the
    /// provider-visible prompt surface stale.
    pub plugin_prompt_sections: Vec<PromptSectionDescriptor>,
    /// Ordered model-visible tool presentations.
    pub tool_presentations: Vec<ToolPresentationDescriptor>,
    /// Exact source-derived tool presentations from immutable plugin bundles.
    /// They remain distinct from trusted host tools so a resolver can reject a
    /// missing capability binding instead of advertising a non-executable
    /// tool.
    pub plugin_tool_presentations: Vec<ToolPresentationDescriptor>,
    /// Immutable hook descriptor identity.
    pub hook_bundle_digest: Digest,
    /// Exact host capability bindings.
    pub capability_bindings: Vec<CapabilityBindingRef>,
    /// Frozen resource ceilings.
    pub resource_limits: HarnessResourceLimits,
    /// Stable compaction policy descriptor identity.
    pub compaction_policy_digest: Digest,
    /// Stable tool projection descriptor identity.
    pub tool_projection_digest: Digest,
    /// Stable failure policy descriptor identity.
    pub failure_policy_digest: Digest,
}

/// Complete immutable resolved harness snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessSnapshotV1 {
    /// Canonical immutable snapshot identity.
    pub id: HarnessSnapshotId,
    /// Snapshot schema version.
    pub schema_version: u16,
    /// Luau extension ABI selected by this snapshot.
    pub luau_abi_version: u16,
    /// Exact source/configuration specification.
    pub spec: HarnessSnapshotSpec,
    /// Separate stable surface identities.
    pub fingerprints: HarnessSurfaceFingerprints,
}

/// Immutable lineage revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessRevisionV1 {
    /// Canonical revision identity.
    pub revision_id: HarnessRevisionId,
    /// Selected immutable snapshot.
    pub snapshot_id: HarnessSnapshotId,
    /// Zero, one, or more parent lineage heads.
    pub parent_revision_ids: Vec<HarnessRevisionId>,
    /// Revision creator.
    pub actor: HarnessActor,
    /// Immutable transition reason.
    pub reason: HarnessRevisionReason,
    /// Staged candidate that produced this revision, if any.
    pub candidate_id: Option<HarnessCandidateId>,
    /// Metadata timestamp supplied by the caller; excluded from provider surface.
    pub created_at_ms: u64,
}

/// Candidate hypothesis required before model-initiated activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateHypothesis {
    /// Specific observed failure or risk.
    pub targeted_evidence: String,
    /// Claimed desired behavior change.
    pub expected_effect: String,
    /// Explicit regression risk to test.
    pub regression_risk: String,
}

/// Structured registry change instead of editable registry-file text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryOperation {
    /// Add an already-staged local plugin to the ordered registry tail.
    Add { plugin_id: String },
    /// Remove one local plugin from the registry.
    Remove { plugin_id: String },
}

/// Immutable staged candidate input.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessCandidateDraft {
    /// Parent revision against which the proposal was authored.
    pub parent_revision_id: HarnessRevisionId,
    /// Fully recomputed proposed snapshot.
    pub proposed_snapshot_id: HarnessSnapshotId,
    /// Candidate author.
    pub actor: HarnessActor,
    /// Owning session operation, if this is a local proposal.
    pub operation_id: Option<OperationId>,
    /// Stable tool invocation identity supplied by the host, if any.
    pub tool_invocation_id: Option<String>,
    /// Required behavior/evidence/risk declaration.
    pub hypothesis: CandidateHypothesis,
    /// Canonical changed paths.
    pub changed_paths: Vec<NormalizedPath>,
    /// Structured registry updates.
    pub registry_operations: Vec<RegistryOperation>,
    /// Explicit affected surfaces.
    pub changed_surfaces: BTreeSet<HarnessSurface>,
    /// Stable references to target failure signatures.
    pub targeted_failures: Vec<String>,
    /// Stable evidence object references.
    pub evidence: Vec<String>,
    /// Expected externally visible effects.
    pub expected_effects: Vec<String>,
    /// Regression risks to evaluate.
    pub regression_risks: Vec<String>,
    /// Maximum session capability names that this candidate may use.
    pub capability_ceiling: BTreeSet<String>,
}

/// Immutable staged candidate with its deterministic validation result.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessCandidateV1 {
    /// Canonical candidate identity.
    pub candidate_id: HarnessCandidateId,
    /// Original immutable draft fields.
    pub draft: HarnessCandidateDraft,
    /// Validation performed before the candidate became addressable.
    pub validation: CandidateValidation,
}

/// Deterministic candidate validation evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateValidation {
    /// Whether all required static checks passed.
    pub accepted: bool,
    /// Whether the proposed snapshot equals its parent snapshot.
    pub is_noop: bool,
    /// Bounded diagnostics suitable for model-facing tool output.
    pub diagnostics: Vec<String>,
}

/// In-memory index over immutable objects. Exact source bytes remain in the
/// caller-owned artifact store, so a persistent embedding may rebuild this
/// index from its catalog without changing object identity.
pub struct HarnessRepository {
    artifacts: Arc<dyn ArtifactStore>,
    extension_engine: Arc<dyn ExtensionEngine>,
    trees: BTreeMap<HarnessTreeId, HarnessTree>,
    snapshots: BTreeMap<HarnessSnapshotId, HarnessSnapshotV1>,
    revisions: BTreeMap<HarnessRevisionId, HarnessRevisionV1>,
    candidates: BTreeMap<HarnessCandidateId, HarnessCandidateV1>,
}

impl std::fmt::Debug for HarnessRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessRepository")
            .field("tree_count", &self.trees.len())
            .field("snapshot_count", &self.snapshots.len())
            .field("revision_count", &self.revisions.len())
            .field("candidate_count", &self.candidates.len())
            .finish_non_exhaustive()
    }
}

/// Errors at immutable harness-lineage boundaries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessLineageError {
    /// A source or descriptor violates the canonical contract.
    Invalid { message: String },
    /// Immutable artifact storage rejected a source blob.
    Artifact { message: String },
    /// An ID was referenced before it entered the repository index.
    NotFound { kind: &'static str, id: String },
    /// A requested activation violates candidate/lineage state.
    InvalidActivation { message: String },
}

impl fmt::Display for HarnessLineageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { message } => {
                write!(formatter, "invalid harness lineage input: {message}")
            }
            Self::Artifact { message } => {
                write!(formatter, "harness source artifact error: {message}")
            }
            Self::NotFound { kind, id } => write!(formatter, "unknown {kind} {id}"),
            Self::InvalidActivation { message } => {
                write!(formatter, "invalid harness activation: {message}")
            }
        }
    }
}

impl std::error::Error for HarnessLineageError {}

impl HarnessRepository {
    /// Construct an empty repository over a caller-owned immutable object
    /// store and an explicit closed-source extension engine.
    pub fn with_extension_engine(
        artifacts: Arc<dyn ArtifactStore>,
        extension_engine: Arc<dyn ExtensionEngine>,
    ) -> Self {
        Self {
            artifacts,
            extension_engine,
            trees: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            revisions: BTreeMap::new(),
            candidates: BTreeMap::new(),
        }
    }

    pub(crate) fn extension_engine(&self) -> Arc<dyn ExtensionEngine> {
        Arc::clone(&self.extension_engine)
    }

    /// Return source-artifact roots reachable from every retained immutable
    /// snapshot. This includes active, rollback, and rejected-candidate
    /// snapshot inputs because the repository deliberately preserves complete
    /// lineage for inspection and recovery. A caller combines these with the
    /// session's direct artifact roots before running `tea-session` GC.
    pub fn artifact_roots(&self) -> BTreeSet<ArtifactId> {
        let mut roots = BTreeSet::new();
        for snapshot in self.snapshots.values() {
            for plugin in snapshot
                .spec
                .ordered_global_plugins
                .iter()
                .chain(snapshot.spec.ordered_session_plugins.iter())
            {
                if let Some(tree) = self.trees.get(&plugin.tree_id) {
                    roots.extend(tree.files.values().map(|file| file.artifact_id));
                }
            }
        }
        roots
    }

    /// Store exact source bytes and return their canonical immutable tree.
    pub fn stage_tree(
        &mut self,
        files: impl IntoIterator<Item = (NormalizedPath, Vec<u8>, String)>,
        limits: &HarnessTreeLimits,
    ) -> Result<HarnessTree, HarnessLineageError> {
        if limits.maximum_files == 0 || limits.maximum_source_bytes == 0 {
            return Err(HarnessLineageError::Invalid {
                message: "tree limits must be greater than zero".into(),
            });
        }
        let mut staged = BTreeMap::new();
        let mut folded = BTreeSet::new();
        let mut total = 0_usize;
        for (path, bytes, media_type) in files {
            if staged.len() >= limits.maximum_files {
                return Err(HarnessLineageError::Invalid {
                    message: "source tree exceeds its file-count limit".into(),
                });
            }
            total = total.saturating_add(bytes.len());
            if total > limits.maximum_source_bytes {
                return Err(HarnessLineageError::Invalid {
                    message: "source tree exceeds its total source-byte limit".into(),
                });
            }
            let case_folded = path.as_str().to_ascii_lowercase();
            if !folded.insert(case_folded) {
                return Err(HarnessLineageError::Invalid {
                    message: format!("source tree has a case-insensitive path collision at {path}"),
                });
            }
            if staged.contains_key(&path) {
                return Err(HarnessLineageError::Invalid {
                    message: format!("source tree repeats canonical path {path}"),
                });
            }
            let descriptor = self.artifacts.put(&bytes, &media_type).map_err(|error| {
                HarnessLineageError::Artifact {
                    message: error.to_string(),
                }
            })?;
            staged.insert(
                path.clone(),
                HarnessTreeFile {
                    path,
                    artifact_id: descriptor.artifact_id,
                    byte_len: descriptor.byte_len,
                    media_type: descriptor.media_type,
                },
            );
        }
        if staged.is_empty() {
            return Err(HarnessLineageError::Invalid {
                message: "source tree cannot be empty".into(),
            });
        }
        let id = tree_id(&staged)?;
        let tree = HarnessTree {
            id: id.clone(),
            files: staged,
        };
        match self.trees.get(&id) {
            Some(existing) if existing != &tree => {
                return Err(HarnessLineageError::Invalid {
                    message: "tree digest collision with different immutable metadata".into(),
                });
            }
            Some(_) => {}
            None => {
                self.trees.insert(id, tree.clone());
            }
        }
        Ok(tree)
    }

    /// Resolve and insert a canonical immutable snapshot.
    pub fn stage_snapshot(
        &mut self,
        mut spec: HarnessSnapshotSpec,
    ) -> Result<HarnessSnapshotV1, HarnessLineageError> {
        populate_plugin_surfaces(
            &mut spec,
            &self.trees,
            self.artifacts.as_ref(),
            self.extension_engine.as_ref(),
        )?;
        validate_snapshot_spec(
            &spec,
            &self.trees,
            self.artifacts.as_ref(),
            self.extension_engine.as_ref(),
        )?;
        let fingerprints = fingerprints(&spec)?;
        let id = snapshot_id(&spec, &fingerprints)?;
        let snapshot = HarnessSnapshotV1 {
            id: id.clone(),
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            luau_abi_version: EXTENSION_ABI_VERSION,
            spec,
            fingerprints,
        };
        match self.snapshots.get(&id) {
            Some(existing) if existing != &snapshot => {
                return Err(HarnessLineageError::Invalid {
                    message: "snapshot digest collision with different immutable metadata".into(),
                });
            }
            Some(_) => {}
            None => {
                self.snapshots.insert(id, snapshot.clone());
            }
        }
        Ok(snapshot)
    }

    /// Seed an initial immutable revision used to pin a new session.
    pub fn seed_revision(
        &mut self,
        snapshot_id: HarnessSnapshotId,
        actor: HarnessActor,
        created_at_ms: u64,
    ) -> Result<HarnessRevisionV1, HarnessLineageError> {
        validate_active_snapshot(self.require_snapshot(&snapshot_id)?)?;
        self.insert_revision(
            snapshot_id,
            Vec::new(),
            actor,
            HarnessRevisionReason::Initial,
            None,
            created_at_ms,
        )
    }

    /// Stage and validate a candidate without mutating any active revision.
    pub fn stage_candidate(
        &mut self,
        draft: HarnessCandidateDraft,
    ) -> Result<HarnessCandidateV1, HarnessLineageError> {
        let parent = self.require_revision(&draft.parent_revision_id)?.clone();
        let snapshot = self.require_snapshot(&draft.proposed_snapshot_id)?.clone();
        let validation = validate_candidate(&draft, &parent, &snapshot, &self.trees)?;
        let candidate_id = candidate_id(&draft)?;
        let candidate = HarnessCandidateV1 {
            candidate_id: candidate_id.clone(),
            draft,
            validation,
        };
        match self.candidates.get(&candidate_id) {
            Some(existing) if existing != &candidate => {
                return Err(HarnessLineageError::Invalid {
                    message: "candidate digest collision with different immutable metadata".into(),
                });
            }
            Some(_) => {}
            None => {
                self.candidates.insert(candidate_id, candidate.clone());
            }
        }
        Ok(candidate)
    }

    /// Turn one accepted non-noop candidate into an immutable child revision.
    pub fn activate_candidate(
        &mut self,
        candidate_id: &HarnessCandidateId,
        actor: HarnessActor,
        created_at_ms: u64,
    ) -> Result<HarnessRevisionV1, HarnessLineageError> {
        let candidate = self.require_candidate(candidate_id)?.clone();
        if !candidate.validation.accepted {
            return Err(HarnessLineageError::InvalidActivation {
                message: "candidate validation did not pass".into(),
            });
        }
        if candidate.validation.is_noop {
            return Err(HarnessLineageError::InvalidActivation {
                message: "a no-op candidate cannot emit a misleading activation".into(),
            });
        }
        let reason = if self.snapshot_is_ancestor(
            &candidate.draft.parent_revision_id,
            &candidate.draft.proposed_snapshot_id,
        )? {
            HarnessRevisionReason::Rollback
        } else {
            HarnessRevisionReason::CandidateActivation
        };
        self.insert_revision(
            candidate.draft.proposed_snapshot_id.clone(),
            vec![candidate.draft.parent_revision_id.clone()],
            actor,
            reason,
            Some(candidate.candidate_id),
            created_at_ms,
        )
    }

    /// Select an earlier immutable snapshot through an ordinary child revision.
    pub fn rollback(
        &mut self,
        current_revision_id: &HarnessRevisionId,
        target_revision_id: &HarnessRevisionId,
        actor: HarnessActor,
        created_at_ms: u64,
    ) -> Result<HarnessRevisionV1, HarnessLineageError> {
        self.require_revision(current_revision_id)?;
        let target = self.require_revision(target_revision_id)?.clone();
        self.insert_revision(
            target.snapshot_id,
            vec![current_revision_id.clone()],
            actor,
            HarnessRevisionReason::Rollback,
            None,
            created_at_ms,
        )
    }

    /// Borrow an immutable staged tree.
    pub fn tree(&self, id: &HarnessTreeId) -> Option<&HarnessTree> {
        self.trees.get(id)
    }

    /// Materialize exact immutable source files from one staged tree.  A
    /// missing or digest-mismatched object fails closed instead of allowing a
    /// candidate to be based on an unverifiable worktree projection.
    pub fn tree_source_files(
        &self,
        id: &HarnessTreeId,
    ) -> Result<Vec<HarnessSourceFile>, HarnessLineageError> {
        let tree = self
            .trees
            .get(id)
            .ok_or_else(|| HarnessLineageError::NotFound {
                kind: "harness tree",
                id: id.to_string(),
            })?;
        tree.files
            .values()
            .map(|file| {
                Ok(HarnessSourceFile {
                    path: file.path.clone(),
                    artifact_id: file.artifact_id,
                    bytes: load_tree_file(self.artifacts.as_ref(), file)?,
                    media_type: file.media_type.clone(),
                })
            })
            .collect()
    }

    /// Read immutable manifest-declared capability names for a plugin in one
    /// staged tree.  This is used while applying structured registry changes;
    /// model input never supplies a replacement capability set separately.
    pub fn plugin_capabilities(
        &self,
        tree_id: &HarnessTreeId,
        plugin_id: &str,
        resource_limits: &HarnessResourceLimits,
    ) -> Result<BTreeSet<String>, HarnessLineageError> {
        let tree = self
            .trees
            .get(tree_id)
            .ok_or_else(|| HarnessLineageError::NotFound {
                kind: "harness tree",
                id: tree_id.to_string(),
            })?;
        let source = extension_source_tree(
            tree,
            plugin_id,
            None,
            resource_limits,
            self.artifacts.as_ref(),
        )?;
        self.extension_engine
            .describe(&source)
            .map(|descriptor| descriptor.requested_capabilities)
            .map_err(extension_error)
    }

    /// Borrow an immutable resolved snapshot.
    pub fn snapshot(&self, id: &HarnessSnapshotId) -> Option<&HarnessSnapshotV1> {
        self.snapshots.get(id)
    }

    /// Borrow an immutable lineage revision.
    pub fn revision(&self, id: &HarnessRevisionId) -> Option<&HarnessRevisionV1> {
        self.revisions.get(id)
    }

    /// Borrow an immutable staged candidate.
    pub fn candidate(&self, id: &HarnessCandidateId) -> Option<&HarnessCandidateV1> {
        self.candidates.get(id)
    }

    /// Materialize every extension source selected by an immutable snapshot in
    /// deterministic global-then-session registry order. The selected
    /// extension engine recompiles this closed source during resolution, so a
    /// missing or altered blob faults closed before a hook or extension tool
    /// can run after process recovery.
    pub(crate) fn load_extension_sources(
        &self,
        snapshot: &HarnessSnapshotV1,
    ) -> Result<Vec<LoadedExtensionSource>, HarnessLineageError> {
        snapshot
            .spec
            .ordered_global_plugins
            .iter()
            .chain(snapshot.spec.ordered_session_plugins.iter())
            .map(|bundle| {
                let tree = self.trees.get(&bundle.tree_id).ok_or_else(|| {
                    HarnessLineageError::NotFound {
                        kind: "plugin source tree",
                        id: bundle.tree_id.to_string(),
                    }
                })?;
                Ok(LoadedExtensionSource {
                    plugin: bundle.clone(),
                    source: extension_source_tree(
                        tree,
                        &bundle.plugin_id,
                        Some(bundle.requested_capabilities.clone()),
                        &snapshot.spec.resource_limits,
                        self.artifacts.as_ref(),
                    )?,
                })
            })
            .collect()
    }

    /// Return immutable revisions in canonical identity order.
    pub fn revisions(&self) -> impl Iterator<Item = &HarnessRevisionV1> {
        self.revisions.values()
    }

    /// Return staged candidates in canonical identity order.
    pub fn candidates(&self) -> impl Iterator<Item = &HarnessCandidateV1> {
        self.candidates.values()
    }

    fn insert_revision(
        &mut self,
        snapshot_id: HarnessSnapshotId,
        parent_revision_ids: Vec<HarnessRevisionId>,
        actor: HarnessActor,
        reason: HarnessRevisionReason,
        candidate_id: Option<HarnessCandidateId>,
        created_at_ms: u64,
    ) -> Result<HarnessRevisionV1, HarnessLineageError> {
        validate_active_snapshot(self.require_snapshot(&snapshot_id)?)?;
        let revision_id = revision_id(
            &snapshot_id,
            &parent_revision_ids,
            actor,
            &reason,
            candidate_id.as_ref(),
        )?;
        let revision = HarnessRevisionV1 {
            revision_id: revision_id.clone(),
            snapshot_id,
            parent_revision_ids,
            actor,
            reason,
            candidate_id,
            created_at_ms,
        };
        match self.revisions.get(&revision_id) {
            // Revision identity intentionally excludes commit-time metadata.
            // Replaying an activation after a crash must therefore retain the
            // first committed timestamp rather than treating a later recovery
            // clock value as a conflicting immutable object.
            Some(existing) => return Ok(existing.clone()),
            None => {
                self.revisions.insert(revision_id, revision.clone());
            }
        }
        Ok(revision)
    }

    fn require_snapshot(
        &self,
        id: &HarnessSnapshotId,
    ) -> Result<&HarnessSnapshotV1, HarnessLineageError> {
        self.snapshots
            .get(id)
            .ok_or_else(|| HarnessLineageError::NotFound {
                kind: "harness snapshot",
                id: id.to_string(),
            })
    }

    fn require_revision(
        &self,
        id: &HarnessRevisionId,
    ) -> Result<&HarnessRevisionV1, HarnessLineageError> {
        self.revisions
            .get(id)
            .ok_or_else(|| HarnessLineageError::NotFound {
                kind: "harness revision",
                id: id.to_string(),
            })
    }

    fn require_candidate(
        &self,
        id: &HarnessCandidateId,
    ) -> Result<&HarnessCandidateV1, HarnessLineageError> {
        self.candidates
            .get(id)
            .ok_or_else(|| HarnessLineageError::NotFound {
                kind: "harness candidate",
                id: id.to_string(),
            })
    }

    fn snapshot_is_ancestor(
        &self,
        revision_id: &HarnessRevisionId,
        snapshot_id: &HarnessSnapshotId,
    ) -> Result<bool, HarnessLineageError> {
        let mut pending = vec![revision_id.clone()];
        let mut visited = BTreeSet::new();
        while let Some(current) = pending.pop() {
            if !visited.insert(current.clone()) {
                continue;
            }
            let revision = self.require_revision(&current)?;
            if &revision.snapshot_id == snapshot_id {
                return Ok(true);
            }
            pending.extend(revision.parent_revision_ids.iter().cloned());
        }
        Ok(false)
    }
}

fn validate_snapshot_spec(
    spec: &HarnessSnapshotSpec,
    trees: &BTreeMap<HarnessTreeId, HarnessTree>,
    artifacts: &dyn ArtifactStore,
    extension_engine: &dyn ExtensionEngine,
) -> Result<(), HarnessLineageError> {
    if spec.resource_limits.source_bytes == 0
        || spec.resource_limits.memory_bytes == 0
        || spec.resource_limits.instruction_checks == 0
        || spec.resource_limits.provider_surface_bytes == 0
    {
        return Err(HarnessLineageError::Invalid {
            message: "harness resource limits must all be greater than zero".into(),
        });
    }
    let expected_plugin_surfaces =
        collect_plugin_surfaces(spec, trees, artifacts, extension_engine)?;
    if spec.plugin_prompt_sections != expected_plugin_surfaces.prompt_sections
        || spec.plugin_tool_presentations != expected_plugin_surfaces.tool_presentations
    {
        return Err(HarnessLineageError::Invalid {
            message: "snapshot source-derived plugin surfaces disagree with its closed bundles"
                .into(),
        });
    }
    let mut sections = BTreeSet::new();
    for section in spec
        .prompt_sections
        .iter()
        .chain(spec.plugin_prompt_sections.iter())
    {
        validate_label(&section.id, "prompt section")?;
        if !sections.insert(section.id.clone()) {
            return Err(HarnessLineageError::Invalid {
                message: format!("duplicate prompt section {}", section.id),
            });
        }
    }
    let mut tool_names = BTreeSet::new();
    for tool in spec
        .tool_presentations
        .iter()
        .chain(spec.plugin_tool_presentations.iter())
    {
        validate_label(&tool.name, "tool name")?;
        if !tool_names.insert(tool.name.clone()) {
            return Err(HarnessLineageError::Invalid {
                message: format!("duplicate model-visible tool {}", tool.name),
            });
        }
        if is_reserved_tool_name(&tool.name) {
            return Err(HarnessLineageError::Invalid {
                message: format!("tool {} is reserved by the durable host", tool.name),
            });
        }
        if !matches!(tool.execution_mode.as_str(), "sequential" | "parallel") {
            return Err(HarnessLineageError::Invalid {
                message: format!("tool {} has an unknown execution mode", tool.name),
            });
        }
        if !matches!(
            tool.cancellation_settlement_mode.as_str(),
            "drop_future" | "await_future"
        ) {
            return Err(HarnessLineageError::Invalid {
                message: format!(
                    "tool {} has an unknown cancellation settlement mode",
                    tool.name
                ),
            });
        }
        let _ = tool
            .schema
            .to_json_string()
            .map_err(|error| HarnessLineageError::Invalid {
                message: format!("tool {} schema cannot encode: {error}", tool.name),
            })?;
    }
    let mut bindings = BTreeSet::new();
    for binding in &spec.capability_bindings {
        validate_plugin_id(&binding.plugin_id)?;
        validate_label(&binding.capability, "capability")?;
        validate_label(&binding.capability_version, "capability version")?;
        if !bindings.insert((binding.plugin_id.clone(), binding.capability.clone())) {
            return Err(HarnessLineageError::Invalid {
                message: format!(
                    "plugin {} capability {} is bound more than once",
                    binding.plugin_id, binding.capability
                ),
            });
        }
    }
    let prompt = compose_system_prompt(spec);
    if prompt.len() > spec.resource_limits.provider_surface_bytes {
        return Err(HarnessLineageError::Invalid {
            message: "provider-visible system prompt exceeds the frozen byte limit".into(),
        });
    }
    Ok(())
}

/// A candidate snapshot may record a requested authority that has no host
/// binding so the request remains visible for manual review. Selecting a
/// snapshot for an actual revision is stricter: every request must already
/// have an exact immutable binding.
fn validate_active_snapshot(snapshot: &HarnessSnapshotV1) -> Result<(), HarnessLineageError> {
    let bindings = snapshot
        .spec
        .capability_bindings
        .iter()
        .map(|binding| (binding.plugin_id.as_str(), binding.capability.as_str()))
        .collect::<BTreeSet<_>>();
    for bundle in snapshot
        .spec
        .ordered_global_plugins
        .iter()
        .chain(snapshot.spec.ordered_session_plugins.iter())
    {
        for capability in &bundle.requested_capabilities {
            if !bindings.contains(&(bundle.plugin_id.as_str(), capability.as_str())) {
                return Err(HarnessLineageError::InvalidActivation {
                    message: format!(
                        "snapshot {} plugin {} requests capability {} without an immutable host binding",
                        snapshot.id, bundle.plugin_id, capability,
                    ),
                });
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct PluginSurfaces {
    prompt_sections: Vec<PromptSectionDescriptor>,
    tool_presentations: Vec<ToolPresentationDescriptor>,
}

/// Recompute the source-owned provider surfaces before assigning a snapshot
/// identity.  This keeps a source edit from changing a plugin's prompt or
/// tool declaration without also changing the exact serialized provider
/// surface and its cache-domain fingerprint.
fn populate_plugin_surfaces(
    spec: &mut HarnessSnapshotSpec,
    trees: &BTreeMap<HarnessTreeId, HarnessTree>,
    artifacts: &dyn ArtifactStore,
    extension_engine: &dyn ExtensionEngine,
) -> Result<(), HarnessLineageError> {
    let surfaces = collect_plugin_surfaces(spec, trees, artifacts, extension_engine)?;
    spec.plugin_prompt_sections = surfaces.prompt_sections;
    spec.plugin_tool_presentations = surfaces.tool_presentations;
    Ok(())
}

fn collect_plugin_surfaces(
    spec: &HarnessSnapshotSpec,
    trees: &BTreeMap<HarnessTreeId, HarnessTree>,
    artifacts: &dyn ArtifactStore,
    extension_engine: &dyn ExtensionEngine,
) -> Result<PluginSurfaces, HarnessLineageError> {
    let mut plugin_ids = BTreeSet::new();
    let mut plugin_tool_names = BTreeSet::new();
    let mut surfaces = PluginSurfaces::default();
    for bundle in spec
        .ordered_global_plugins
        .iter()
        .chain(spec.ordered_session_plugins.iter())
    {
        validate_plugin_id(&bundle.plugin_id)?;
        if !plugin_ids.insert(bundle.plugin_id.clone()) {
            return Err(HarnessLineageError::Invalid {
                message: format!("plugin {} is registered more than once", bundle.plugin_id),
            });
        }
        let tree = trees
            .get(&bundle.tree_id)
            .ok_or_else(|| HarnessLineageError::NotFound {
                kind: "plugin source tree",
                id: bundle.tree_id.to_string(),
            })?;
        let source = extension_source_tree(
            tree,
            &bundle.plugin_id,
            Some(bundle.requested_capabilities.clone()),
            &spec.resource_limits,
            artifacts,
        )?;
        let descriptor = extension_engine
            .describe(&source)
            .map_err(extension_error)?;
        for section in descriptor.prompt_sections {
            surfaces.prompt_sections.push(PromptSectionDescriptor {
                id: format!("{}.{}", bundle.plugin_id, section.id),
                content: section.content,
            });
        }
        for tool in descriptor.tools {
            if !bundle.requested_capabilities.contains(&tool.capability) {
                return Err(HarnessLineageError::Invalid {
                    message: format!(
                        "plugin {} tool {} names undeclared capability {}",
                        bundle.plugin_id, tool.name, tool.capability
                    ),
                });
            }
            if !plugin_tool_names.insert(tool.name.clone()) {
                return Err(HarnessLineageError::Invalid {
                    message: format!("plugins declare duplicate tool {}", tool.name),
                });
            }
            if is_reserved_tool_name(&tool.name) {
                return Err(HarnessLineageError::Invalid {
                    message: format!(
                        "plugin {} declares host-reserved tool {}",
                        bundle.plugin_id, tool.name
                    ),
                });
            }
            surfaces
                .tool_presentations
                .push(ToolPresentationDescriptor {
                    name: tool.name,
                    description: tool.description,
                    schema: tool.schema,
                    execution_mode: match tool.execution_mode {
                        tea_core::tool::ToolExecutionMode::Sequential => "sequential".into(),
                        tea_core::tool::ToolExecutionMode::Parallel => "parallel".into(),
                    },
                    requires_exclusive_batch: tool.requires_exclusive_batch,
                    cancellation_settlement_mode: match tool.cancellation_settlement_mode {
                        tea_core::tool::CancellationSettlementMode::DropFuture => {
                            "drop_future".into()
                        }
                        tea_core::tool::CancellationSettlementMode::AwaitFuture => {
                            "await_future".into()
                        }
                    },
                });
        }
    }
    Ok(surfaces)
}

/// Materialize one closed UTF-8 extension tree from an immutable harness tree.
/// The extension engine validates the language-specific manifest and module
/// closure; this core lineage code owns only source identity and frozen limits.
fn extension_source_tree(
    tree: &HarnessTree,
    plugin_id: &str,
    expected_capabilities: Option<BTreeSet<String>>,
    resource_limits: &HarnessResourceLimits,
    artifacts: &dyn ArtifactStore,
) -> Result<ExtensionSourceTree, HarnessLineageError> {
    let prefix = format!("plugins/{plugin_id}/");
    let mut files = BTreeMap::new();
    for file in tree
        .files
        .values()
        .filter(|file| file.path.as_str().starts_with(&prefix))
    {
        let relative = file
            .path
            .as_str()
            .strip_prefix(&prefix)
            .expect("source file was filtered by its extension prefix");
        let bytes = load_tree_file(artifacts, file)?;
        let contents = String::from_utf8(bytes).map_err(|_| HarnessLineageError::Invalid {
            message: format!(
                "plugin {} extension source {} is not UTF-8",
                plugin_id, file.path
            ),
        })?;
        files.insert(relative.to_owned(), contents);
    }
    if files.is_empty() {
        return Err(HarnessLineageError::Invalid {
            message: format!("plugin {plugin_id} has no source files"),
        });
    }
    Ok(ExtensionSourceTree {
        extension_id: plugin_id.to_owned(),
        files,
        expected_capabilities,
        limits: ExtensionLimits {
            max_source_bytes: resource_limits.source_bytes,
            max_memory_bytes: resource_limits.memory_bytes,
            max_interrupt_checks: resource_limits.instruction_checks as usize,
        },
    })
}

fn extension_error(error: tea_core::harness::extension::ExtensionError) -> HarnessLineageError {
    HarnessLineageError::Invalid {
        message: format!("extension validation failed: {error}"),
    }
}

fn load_tree_file(
    artifacts: &dyn ArtifactStore,
    file: &HarnessTreeFile,
) -> Result<Vec<u8>, HarnessLineageError> {
    let bytes = artifacts
        .get(file.artifact_id)
        .map_err(|error| HarnessLineageError::Artifact {
            message: error.to_string(),
        })?;
    if bytes.len() as u64 != file.byte_len || ArtifactId::from_bytes(&bytes) != file.artifact_id {
        return Err(HarnessLineageError::Invalid {
            message: format!(
                "harness source object {} does not match immutable tree metadata",
                file.path
            ),
        });
    }
    Ok(bytes)
}

fn is_reserved_tool_name(name: &str) -> bool {
    matches!(
        name,
        "tea_harness" | "tea_artifact_read" | "tea_artifact_search" | "tea_history_search"
    )
}

fn validate_candidate(
    draft: &HarnessCandidateDraft,
    parent: &HarnessRevisionV1,
    snapshot: &HarnessSnapshotV1,
    trees: &BTreeMap<HarnessTreeId, HarnessTree>,
) -> Result<CandidateValidation, HarnessLineageError> {
    let mut diagnostics = Vec::new();
    if draft.hypothesis.targeted_evidence.is_empty()
        || draft.hypothesis.expected_effect.is_empty()
        || draft.hypothesis.regression_risk.is_empty()
    {
        diagnostics.push(
            "candidate hypothesis must name evidence, expected effect, and regression risk".into(),
        );
    }
    let mut paths = BTreeSet::new();
    for path in &draft.changed_paths {
        if !paths.insert(path.clone()) {
            diagnostics.push(format!("candidate repeats changed path {path}"));
        }
    }
    let snapshot_capabilities = snapshot
        .spec
        .ordered_session_plugins
        .iter()
        .flat_map(|bundle| bundle.requested_capabilities.iter().cloned())
        .collect::<BTreeSet<_>>();
    let extra = snapshot_capabilities
        .difference(&draft.capability_ceiling)
        .cloned()
        .collect::<Vec<_>>();
    if !extra.is_empty() {
        diagnostics.push(format!(
            "candidate requests capability outside its frozen ceiling: {}",
            extra.join(", ")
        ));
    }
    let bound_capabilities = snapshot
        .spec
        .capability_bindings
        .iter()
        .map(|binding| (binding.plugin_id.as_str(), binding.capability.as_str()))
        .collect::<BTreeSet<_>>();
    let unbound = snapshot
        .spec
        .ordered_session_plugins
        .iter()
        .flat_map(|bundle| {
            bundle
                .requested_capabilities
                .iter()
                .filter(|&capability| {
                    !bound_capabilities.contains(&(bundle.plugin_id.as_str(), capability.as_str()))
                })
                .map(|capability| format!("{}.{}", bundle.plugin_id, capability))
        })
        .collect::<Vec<_>>();
    if !unbound.is_empty() {
        diagnostics.push(format!(
            "candidate requests session capability without an immutable host binding: {}",
            unbound.join(", ")
        ));
    }
    for bundle in &snapshot.spec.ordered_session_plugins {
        if let Some(tree) = trees.get(&bundle.tree_id)
            && !tree_has_plugin_layout(tree, &bundle.plugin_id)
        {
            diagnostics.push(format!(
                "plugin {} tree does not contain the required closed source layout",
                bundle.plugin_id
            ));
        }
    }
    if draft.changed_surfaces.is_empty() && parent.snapshot_id != draft.proposed_snapshot_id {
        diagnostics.push("non-noop candidate must name at least one changed surface".into());
    }
    let is_noop = parent.snapshot_id == draft.proposed_snapshot_id;
    if is_noop {
        diagnostics.push("candidate recomputes to its parent snapshot and is a no-op".into());
    }
    Ok(CandidateValidation {
        accepted: diagnostics.is_empty(),
        is_noop,
        diagnostics,
    })
}

fn tree_has_plugin_layout(tree: &HarnessTree, plugin_id: &str) -> bool {
    let manifest = format!("plugins/{plugin_id}/manifest.json");
    // First-party checked-in bundles use `init.luau`, while existing authored
    // session plugins use `main.luau`. The language adapter still validates
    // the manifest's exact entrypoint; this structural check only verifies
    // that a candidate retained a closed executable source layout.
    let entrypoints = [
        format!("plugins/{plugin_id}/main.luau"),
        format!("plugins/{plugin_id}/init.luau"),
    ];
    tree.files.keys().any(|path| path.as_str() == manifest)
        && tree.files.keys().any(|path| {
            entrypoints
                .iter()
                .any(|entrypoint| path.as_str() == entrypoint)
        })
}

fn fingerprints(
    spec: &HarnessSnapshotSpec,
) -> Result<HarnessSurfaceFingerprints, HarnessLineageError> {
    let prompt = compose_system_prompt(spec);
    let system_prompt_digest = Digest::from_bytes(prompt.as_bytes());
    let ordered_tool_definitions_digest = digest_tools(&all_tool_presentations(spec))?;
    let tool_execution_policy_digest =
        digest_tool_execution_policies(&all_tool_presentations(spec));
    let capability_bindings_digest = digest_capabilities(&spec.capability_bindings);
    let mut provider =
        CanonicalHashWriter::new("tea-harness-provider-surface-v1", 1, EXTENSION_ABI_VERSION);
    provider.bytes("system_prompt_digest", system_prompt_digest.as_bytes());
    provider.bytes(
        "tool_definitions_digest",
        ordered_tool_definitions_digest.as_bytes(),
    );
    Ok(HarnessSurfaceFingerprints {
        system_prompt_digest,
        ordered_tool_definitions_digest,
        tool_execution_policy_digest,
        hook_bundle_digest: spec.hook_bundle_digest,
        capability_bindings_digest,
        compaction_policy_digest: spec.compaction_policy_digest,
        provider_surface_digest: provider.finish(),
    })
}

pub(crate) fn compose_system_prompt(spec: &HarnessSnapshotSpec) -> String {
    let mut sections = Vec::new();
    sections.push(spec.base_system_prompt.as_str());
    if let Some(addendum) = &spec.self_extension_addendum {
        sections.push(addendum.as_str());
    }
    sections.extend(
        spec.prompt_sections
            .iter()
            .map(|section| section.content.as_str()),
    );
    sections.extend(
        spec.plugin_prompt_sections
            .iter()
            .map(|section| section.content.as_str()),
    );
    sections.join("\n\n")
}

/// Combine the host hook implementation identity with the source-pinned
/// session-plugin hook contribution for one snapshot.
pub(crate) fn runtime_hook_bundle_digest(
    host_identity: Digest,
    spec: &HarnessSnapshotSpec,
) -> Digest {
    if spec.ordered_session_plugins.is_empty() {
        return host_identity;
    }
    let mut writer = CanonicalHashWriter::new("tea-runtime-hook-bundle-v1", 1, 1);
    writer.bytes("host_identity", host_identity.as_bytes());
    writer.bytes(
        "session_plugin_identity",
        session_plugin_hook_digest(spec).as_bytes(),
    );
    writer.finish()
}

/// Canonical identity of the ordered session-plugin hook contribution.
pub(crate) fn session_plugin_hook_digest(spec: &HarnessSnapshotSpec) -> Digest {
    let mut writer = CanonicalHashWriter::new("tea-session-plugin-hooks-v1", 1, 2);
    writer.u64("plugin_count", spec.ordered_session_plugins.len() as u64);
    for plugin in &spec.ordered_session_plugins {
        writer.string("plugin_id", &plugin.plugin_id);
        writer.string("tree_id", plugin.tree_id.as_str());
    }
    writer.finish()
}

fn all_tool_presentations(spec: &HarnessSnapshotSpec) -> Vec<ToolPresentationDescriptor> {
    spec.tool_presentations
        .iter()
        .chain(spec.plugin_tool_presentations.iter())
        .cloned()
        .collect()
}

fn digest_tools(tools: &[ToolPresentationDescriptor]) -> Result<Digest, HarnessLineageError> {
    let mut writer = CanonicalHashWriter::new(
        "tea-harness-tool-presentations-v1",
        1,
        EXTENSION_ABI_VERSION,
    );
    writer.u64("tool_count", tools.len() as u64);
    for tool in tools {
        writer.string("name", &tool.name);
        writer.string("description", &tool.description);
        writer.string(
            "schema",
            &tool
                .schema
                .to_json_string()
                .map_err(|error| HarnessLineageError::Invalid {
                    message: format!("tool {} schema cannot encode: {error}", tool.name),
                })?,
        );
    }
    Ok(writer.finish())
}

fn digest_tool_execution_policies(tools: &[ToolPresentationDescriptor]) -> Digest {
    let mut writer = CanonicalHashWriter::new(
        "tea-harness-tool-execution-policies-v1",
        1,
        EXTENSION_ABI_VERSION,
    );
    writer.u64("tool_count", tools.len() as u64);
    for tool in tools {
        writer.string("name", &tool.name);
        writer.string("execution_mode", &tool.execution_mode);
        writer.boolean("requires_exclusive_batch", tool.requires_exclusive_batch);
        writer.string(
            "cancellation_settlement_mode",
            &tool.cancellation_settlement_mode,
        );
    }
    writer.finish()
}

fn digest_capabilities(bindings: &[CapabilityBindingRef]) -> Digest {
    let mut writer = CanonicalHashWriter::new(
        "tea-harness-capability-bindings-v1",
        1,
        EXTENSION_ABI_VERSION,
    );
    writer.u64("binding_count", bindings.len() as u64);
    for binding in bindings {
        writer.string("plugin_id", &binding.plugin_id);
        writer.string("capability", &binding.capability);
        writer.string("capability_version", &binding.capability_version);
        writer.bytes("binding_digest", binding.binding_digest.as_bytes());
    }
    writer.finish()
}

fn tree_id(
    files: &BTreeMap<NormalizedPath, HarnessTreeFile>,
) -> Result<HarnessTreeId, HarnessLineageError> {
    let mut writer = CanonicalHashWriter::new("tea-harness-tree-v1", 1, EXTENSION_ABI_VERSION);
    writer.u64("file_count", files.len() as u64);
    for file in files.values() {
        writer.normalized_path("path", &file.path);
        writer.string("artifact_id", &file.artifact_id.to_hex());
        writer.u64("byte_len", file.byte_len);
        writer.string("media_type", &file.media_type);
    }
    HarnessTreeId::new(format!("harness-tree-{}", writer.finish().to_hex())).map_err(|error| {
        HarnessLineageError::Invalid {
            message: error.to_string(),
        }
    })
}

fn snapshot_id(
    spec: &HarnessSnapshotSpec,
    fingerprints: &HarnessSurfaceFingerprints,
) -> Result<HarnessSnapshotId, HarnessLineageError> {
    let mut writer = CanonicalHashWriter::new(
        "tea-harness-snapshot-v1",
        SNAPSHOT_SCHEMA_VERSION,
        EXTENSION_ABI_VERSION,
    );
    writer.bytes("base_profile_digest", spec.base_profile_digest.as_bytes());
    writer.string("model_harness_profile", spec.model_harness_profile.as_str());
    writer.string("system_prompt", &compose_system_prompt(spec));
    write_prompt_sections(&mut writer, "trusted", &spec.prompt_sections);
    write_prompt_sections(&mut writer, "plugin", &spec.plugin_prompt_sections);
    write_bundles(&mut writer, "global", &spec.ordered_global_plugins);
    write_bundles(&mut writer, "session", &spec.ordered_session_plugins);
    writer.bytes(
        "provider_surface_digest",
        fingerprints.provider_surface_digest.as_bytes(),
    );
    writer.bytes(
        "tool_execution_policy_digest",
        fingerprints.tool_execution_policy_digest.as_bytes(),
    );
    writer.bytes("hook_bundle_digest", spec.hook_bundle_digest.as_bytes());
    writer.bytes(
        "capability_bindings_digest",
        fingerprints.capability_bindings_digest.as_bytes(),
    );
    writer.bytes(
        "compaction_policy_digest",
        spec.compaction_policy_digest.as_bytes(),
    );
    writer.bytes(
        "tool_projection_digest",
        spec.tool_projection_digest.as_bytes(),
    );
    writer.bytes(
        "failure_policy_digest",
        spec.failure_policy_digest.as_bytes(),
    );
    writer.u64("source_bytes", spec.resource_limits.source_bytes as u64);
    writer.u64("memory_bytes", spec.resource_limits.memory_bytes as u64);
    writer.u64(
        "instruction_checks",
        spec.resource_limits.instruction_checks as u64,
    );
    writer.u64(
        "provider_surface_bytes",
        spec.resource_limits.provider_surface_bytes as u64,
    );
    HarnessSnapshotId::new(format!("harness-snapshot-{}", writer.finish().to_hex())).map_err(
        |error| HarnessLineageError::Invalid {
            message: error.to_string(),
        },
    )
}

fn write_prompt_sections(
    writer: &mut CanonicalHashWriter,
    namespace: &str,
    sections: &[PromptSectionDescriptor],
) {
    writer.u64(
        &format!("{namespace}_prompt_section_count"),
        sections.len() as u64,
    );
    for section in sections {
        writer.string(&format!("{namespace}_prompt_section_id"), &section.id);
        writer.string(
            &format!("{namespace}_prompt_section_content"),
            &section.content,
        );
    }
}

fn write_bundles(writer: &mut CanonicalHashWriter, name: &str, bundles: &[PluginBundleRef]) {
    writer.u64(&format!("{name}_bundle_count"), bundles.len() as u64);
    for bundle in bundles {
        writer.string(&format!("{name}_plugin_id"), &bundle.plugin_id);
        writer.string(&format!("{name}_tree_id"), bundle.tree_id.as_str());
        writer.u64(
            &format!("{name}_capability_count"),
            bundle.requested_capabilities.len() as u64,
        );
        for capability in &bundle.requested_capabilities {
            writer.string(&format!("{name}_capability"), capability);
        }
    }
}

fn revision_id(
    snapshot_id: &HarnessSnapshotId,
    parents: &[HarnessRevisionId],
    actor: HarnessActor,
    reason: &HarnessRevisionReason,
    candidate: Option<&HarnessCandidateId>,
) -> Result<HarnessRevisionId, HarnessLineageError> {
    let mut writer = CanonicalHashWriter::new("tea-harness-revision-v1", 1, EXTENSION_ABI_VERSION);
    writer.string("snapshot_id", snapshot_id.as_str());
    writer.u64("parent_count", parents.len() as u64);
    for parent in parents {
        writer.string("parent_revision_id", parent.as_str());
    }
    writer.discriminant("actor", actor_discriminant(actor));
    writer.discriminant("reason", reason_discriminant(reason));
    writer.boolean("has_candidate", candidate.is_some());
    if let Some(candidate) = candidate {
        writer.string("candidate_id", candidate.as_str());
    }
    HarnessRevisionId::new(format!("harness-revision-{}", writer.finish().to_hex())).map_err(
        |error| HarnessLineageError::Invalid {
            message: error.to_string(),
        },
    )
}

fn candidate_id(draft: &HarnessCandidateDraft) -> Result<HarnessCandidateId, HarnessLineageError> {
    let mut writer = CanonicalHashWriter::new("tea-harness-candidate-v1", 1, EXTENSION_ABI_VERSION);
    writer.string("parent_revision_id", draft.parent_revision_id.as_str());
    writer.string("proposed_snapshot_id", draft.proposed_snapshot_id.as_str());
    writer.discriminant("actor", actor_discriminant(draft.actor));
    writer.boolean("has_operation", draft.operation_id.is_some());
    if let Some(operation) = &draft.operation_id {
        writer.string("operation_id", operation.as_str());
    }
    writer.boolean("has_tool_invocation", draft.tool_invocation_id.is_some());
    if let Some(invocation) = &draft.tool_invocation_id {
        writer.string("tool_invocation_id", invocation);
    }
    writer.string("hypothesis_evidence", &draft.hypothesis.targeted_evidence);
    writer.string("hypothesis_effect", &draft.hypothesis.expected_effect);
    writer.string("hypothesis_risk", &draft.hypothesis.regression_risk);
    writer.u64("changed_path_count", draft.changed_paths.len() as u64);
    for path in &draft.changed_paths {
        writer.normalized_path("changed_path", path);
    }
    writer.u64(
        "registry_operation_count",
        draft.registry_operations.len() as u64,
    );
    for operation in &draft.registry_operations {
        match operation {
            RegistryOperation::Add { plugin_id } => {
                writer.discriminant("registry_operation_kind", 1);
                writer.string("registry_operation_plugin_id", plugin_id);
            }
            RegistryOperation::Remove { plugin_id } => {
                writer.discriminant("registry_operation_kind", 2);
                writer.string("registry_operation_plugin_id", plugin_id);
            }
        }
    }
    writer.u64("surface_count", draft.changed_surfaces.len() as u64);
    for surface in &draft.changed_surfaces {
        writer.discriminant("surface", surface_discriminant(*surface));
    }
    write_candidate_strings(&mut writer, "targeted_failure", &draft.targeted_failures);
    write_candidate_strings(&mut writer, "evidence", &draft.evidence);
    write_candidate_strings(&mut writer, "expected_effect", &draft.expected_effects);
    write_candidate_strings(&mut writer, "regression_risk", &draft.regression_risks);
    writer.u64("ceiling_count", draft.capability_ceiling.len() as u64);
    for capability in &draft.capability_ceiling {
        writer.string("capability_ceiling", capability);
    }
    HarnessCandidateId::new(format!("harness-candidate-{}", writer.finish().to_hex())).map_err(
        |error| HarnessLineageError::Invalid {
            message: error.to_string(),
        },
    )
}

fn write_candidate_strings(writer: &mut CanonicalHashWriter, name: &str, values: &[String]) {
    writer.u64(&format!("{name}_count"), values.len() as u64);
    for value in values {
        writer.string(name, value);
    }
}

fn actor_discriminant(actor: HarnessActor) -> u16 {
    match actor {
        HarnessActor::Host => 1,
        HarnessActor::Operator => 2,
        HarnessActor::Model => 3,
    }
}

fn reason_discriminant(reason: &HarnessRevisionReason) -> u16 {
    match reason {
        HarnessRevisionReason::Initial => 1,
        HarnessRevisionReason::CandidateActivation => 2,
        HarnessRevisionReason::GlobalRebase => 3,
        HarnessRevisionReason::Rollback => 4,
    }
}

fn surface_discriminant(surface: HarnessSurface) -> u16 {
    match surface {
        HarnessSurface::SystemPrompt => 1,
        HarnessSurface::ToolDefinitions => 2,
        HarnessSurface::Hooks => 3,
        HarnessSurface::CapabilityBindings => 4,
        HarnessSurface::Compaction => 5,
        HarnessSurface::ToolProjection => 6,
        HarnessSurface::FailurePolicy => 7,
        HarnessSurface::ToolExecutionPolicy => 8,
    }
}

fn validate_plugin_id(value: &str) -> Result<(), HarnessLineageError> {
    validate_label(value, "plugin ID")
}

fn validate_label(value: &str, kind: &str) -> Result<(), HarnessLineageError> {
    if value.is_empty()
        || value.len() > 120
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    {
        return Err(HarnessLineageError::Invalid {
            message: format!("{kind} must use the portable [A-Za-z0-9._-] spelling"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ToolPresentationDescriptor {
        ToolPresentationDescriptor {
            name: "transaction".into(),
            description: "Commit one transaction.".into(),
            schema: JsonValue::parse(r#"{"type":"object"}"#).expect("valid schema"),
            execution_mode: "parallel".into(),
            requires_exclusive_batch: false,
            cancellation_settlement_mode: "drop_future".into(),
        }
    }

    #[test]
    fn host_execution_policy_does_not_change_provider_tool_digest() {
        let original = tool();
        let mut changed = original.clone();
        changed.execution_mode = "sequential".into();
        changed.requires_exclusive_batch = true;
        changed.cancellation_settlement_mode = "await_future".into();

        assert_eq!(
            digest_tools(std::slice::from_ref(&original)).expect("provider digest"),
            digest_tools(&[changed.clone()]).expect("provider digest")
        );
        assert_ne!(
            digest_tool_execution_policies(&[original]),
            digest_tool_execution_policies(&[changed])
        );
    }
}
