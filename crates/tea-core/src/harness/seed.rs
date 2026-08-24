//! Explicit construction of one initial immutable harness lineage.
//!
//! This builder centralizes the closed source-tree layout and initial
//! snapshot/revision invariants shared by Tea composition roots. It performs
//! no file, provider, model, application, session, or capability discovery.

use super::extension::{ExtensionEngine, ExtensionSourceTree};
use super::lineage::runtime_hook_bundle_digest;
use super::{
    CapabilityBindingRef, HarnessActor, HarnessError, HarnessRepository, HarnessResourceLimits,
    HarnessRevisionV1, HarnessSnapshotSpec, HarnessSnapshotV1, HarnessTreeLimits,
    ModelHarnessProfile, PluginBundleRef, PromptSectionDescriptor, SelfExtensionMode,
    ToolPresentationDescriptor,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use crate::runtime::RuntimePolicyIdentities;
use tea_session::{ArtifactStore, Digest, NormalizedPath};

/// Registry placement for one explicitly supplied immutable extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HarnessSeedExtensionScope {
    Global,
    Session,
}

/// One exact extension source tree and its ordered registry placement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessSeedExtension {
    pub scope: HarnessSeedExtensionScope,
    pub source: ExtensionSourceTree,
}

/// The complete initial lineage produced by [`HarnessSeedBuilder`].
pub struct SeededHarness {
    pub repository: HarnessRepository,
    pub snapshot: HarnessSnapshotV1,
    pub revision: HarnessRevisionV1,
    pub profile: ModelHarnessProfile,
    pub self_extension_mode: SelfExtensionMode,
}

impl std::fmt::Debug for SeededHarness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SeededHarness")
            .field("snapshot", &self.snapshot.id)
            .field("revision", &self.revision.revision_id)
            .field("profile", &self.profile.profile_id)
            .field("self_extension_mode", &self.self_extension_mode)
            .finish_non_exhaustive()
    }
}

/// Explicit values for staging one source tree, snapshot, and initial revision.
pub struct HarnessSeedBuilder {
    artifacts: Arc<dyn ArtifactStore>,
    extension_engine: Arc<dyn ExtensionEngine>,
    base_profile_digest: Digest,
    base_system_prompt: String,
    profile: ModelHarnessProfile,
    self_extension_mode: SelfExtensionMode,
    self_extension_addendum: Option<String>,
    prompt_sections: Vec<PromptSectionDescriptor>,
    extensions: Vec<HarnessSeedExtension>,
    trusted_tool_presentations: Vec<ToolPresentationDescriptor>,
    capability_bindings: Vec<CapabilityBindingRef>,
    resource_limits: HarnessResourceLimits,
    runtime_policies: RuntimePolicyIdentities,
    tree_limits: HarnessTreeLimits,
}

impl std::fmt::Debug for HarnessSeedBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessSeedBuilder")
            .field("profile", &self.profile.profile_id)
            .field("self_extension_mode", &self.self_extension_mode)
            .field("extension_count", &self.extensions.len())
            .field("trusted_tools", &self.trusted_tool_presentations.len())
            .field("capability_bindings", &self.capability_bindings.len())
            .finish_non_exhaustive()
    }
}

impl HarnessSeedBuilder {
    /// Construct a builder from explicit immutable inputs only.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        artifacts: Arc<dyn ArtifactStore>,
        extension_engine: Arc<dyn ExtensionEngine>,
        base_profile_digest: Digest,
        base_system_prompt: impl Into<String>,
        profile: ModelHarnessProfile,
        self_extension_mode: SelfExtensionMode,
        resource_limits: HarnessResourceLimits,
        runtime_policies: RuntimePolicyIdentities,
    ) -> Self {
        Self {
            artifacts,
            extension_engine,
            base_profile_digest,
            base_system_prompt: base_system_prompt.into(),
            profile,
            self_extension_mode,
            self_extension_addendum: None,
            prompt_sections: Vec::new(),
            extensions: Vec::new(),
            trusted_tool_presentations: Vec::new(),
            capability_bindings: Vec::new(),
            resource_limits,
            runtime_policies,
            tree_limits: HarnessTreeLimits::default(),
        }
    }

    /// Install the explicit addendum paired with an enabled self-extension mode.
    pub fn self_extension_addendum(mut self, addendum: Option<String>) -> Self {
        self.self_extension_addendum = addendum;
        self
    }

    /// Install trusted prompt sections that are not extension-owned.
    pub fn prompt_sections(mut self, sections: Vec<PromptSectionDescriptor>) -> Self {
        self.prompt_sections = sections;
        self
    }

    /// Install extensions in exact registry order.
    pub fn extensions(mut self, extensions: Vec<HarnessSeedExtension>) -> Self {
        self.extensions = extensions;
        self
    }

    /// Install exact trusted host tool presentations in model-visible order.
    pub fn trusted_tool_presentations(mut self, tools: Vec<ToolPresentationDescriptor>) -> Self {
        self.trusted_tool_presentations = tools;
        self
    }

    /// Install exact immutable capability binding references.
    pub fn capability_bindings(mut self, bindings: Vec<CapabilityBindingRef>) -> Self {
        self.capability_bindings = bindings;
        self
    }

    /// Override source-tree staging limits explicitly.
    pub fn tree_limits(mut self, limits: HarnessTreeLimits) -> Self {
        self.tree_limits = limits;
        self
    }

    /// Stage the complete initial lineage without creating a session.
    pub fn seed(
        self,
        actor: HarnessActor,
        created_at_ms: u64,
    ) -> Result<SeededHarness, HarnessError> {
        self.profile.verify_identity()?;
        validate_self_extension(
            self.self_extension_mode,
            self.self_extension_addendum.as_deref(),
        )?;
        let mut repository =
            HarnessRepository::with_extension_engine(self.artifacts, self.extension_engine);
        let mut global_plugins = Vec::new();
        let mut session_plugins = Vec::new();
        let mut extension_ids = BTreeSet::new();
        for extension in self.extensions {
            let source = extension.source;
            if !extension_ids.insert(source.extension_id.clone()) {
                return Err(HarnessError::invalid_state(format!(
                    "harness seed repeats extension {}",
                    source.extension_id,
                )));
            }
            validate_extension_limits(&source, &self.resource_limits)?;
            let requested_capabilities = source.expected_capabilities.clone().ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "harness seed extension {} requires an explicit capability set",
                    source.extension_id,
                ))
            })?;
            let files = source
                .files
                .into_iter()
                .map(|(relative, contents)| {
                    let path =
                        NormalizedPath::new(format!("plugins/{}/{relative}", source.extension_id,))
                            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
                    let media_type = if relative == "manifest.json" {
                        "application/json"
                    } else if relative.ends_with(".luau") {
                        "text/plain"
                    } else {
                        return Err(HarnessError::invalid_state(format!(
                            "harness seed extension {} has unsupported source path {relative}",
                            source.extension_id,
                        )));
                    };
                    Ok((path, contents.into_bytes(), media_type.to_owned()))
                })
                .collect::<Result<Vec<_>, HarnessError>>()?;
            let tree = repository
                .stage_tree(files, &self.tree_limits)
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let bundle = PluginBundleRef {
                plugin_id: source.extension_id,
                tree_id: tree.id,
                requested_capabilities,
            };
            match extension.scope {
                HarnessSeedExtensionScope::Global => global_plugins.push(bundle),
                HarnessSeedExtensionScope::Session => session_plugins.push(bundle),
            }
        }
        let mut snapshot_spec = HarnessSnapshotSpec {
            base_profile_digest: self.base_profile_digest,
            base_system_prompt: self.base_system_prompt,
            model_harness_profile: self.profile.profile_id.clone(),
            self_extension_addendum: self.self_extension_addendum,
            ordered_global_plugins: global_plugins,
            ordered_session_plugins: session_plugins,
            prompt_sections: self.prompt_sections,
            plugin_prompt_sections: Vec::new(),
            tool_presentations: self.trusted_tool_presentations,
            plugin_tool_presentations: Vec::new(),
            hook_bundle_digest: self.runtime_policies.hook_bundle_digest,
            capability_bindings: self.capability_bindings,
            resource_limits: self.resource_limits,
            compaction_policy_digest: self.runtime_policies.compaction_policy_digest,
            tool_projection_digest: self.runtime_policies.tool_projection_digest,
            failure_policy_digest: self.runtime_policies.failure_policy_digest,
        };
        snapshot_spec.hook_bundle_digest = runtime_hook_bundle_digest(
            self.runtime_policies.hook_bundle_digest,
            &snapshot_spec,
        );
        let snapshot = repository
            .stage_snapshot(snapshot_spec)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let revision = repository
            .seed_revision(snapshot.id.clone(), actor, created_at_ms)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        Ok(SeededHarness {
            repository,
            snapshot,
            revision,
            profile: self.profile,
            self_extension_mode: self.self_extension_mode,
        })
    }
}

fn validate_self_extension(
    mode: SelfExtensionMode,
    addendum: Option<&str>,
) -> Result<(), HarnessError> {
    if mode == SelfExtensionMode::Off && addendum.is_some() {
        return Err(HarnessError::invalid_state(
            "an off harness seed cannot carry a self-extension addendum",
        ));
    }
    if mode.exposes_control_tool() && addendum.is_none_or(str::is_empty) {
        return Err(HarnessError::invalid_state(
            "an enabled harness seed requires an explicit self-extension addendum",
        ));
    }
    Ok(())
}

fn validate_extension_limits(
    source: &ExtensionSourceTree,
    limits: &HarnessResourceLimits,
) -> Result<(), HarnessError> {
    if source.limits.max_source_bytes != limits.source_bytes
        || source.limits.max_memory_bytes != limits.memory_bytes
        || source.limits.max_interrupt_checks != limits.instruction_checks as usize
    {
        return Err(HarnessError::invalid_state(format!(
            "harness seed extension {} limits disagree with the immutable snapshot",
            source.extension_id,
        )));
    }
    Ok(())
}
