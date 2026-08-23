//! Canonical persistence codec for immutable harness lineage.
//!
//! The session crate deliberately does not depend on harness types.  This
//! child module therefore encodes the repository index as one strict,
//! content-addressed JSON manifest, while exact source bytes remain in the
//! caller-owned artifact store.  Restore recomputes every source-derived
//! snapshot and identity rather than trusting the manifest merely because it
//! was found in a session object directory.

use super::*;
use tea_session::{
    ArtifactId, Digest, HarnessCandidateId, HarnessRevisionId, HarnessSnapshotId, HarnessTreeId,
    ModelHarnessProfileId, NormalizedPath, OperationId,
};

const CATALOG_SCHEMA_VERSION: u64 = 1;

impl HarnessRepository {
    /// Encode the complete immutable repository index into canonical JSON.
    ///
    /// The returned value has no mutable active pointer.  Activation remains
    /// represented only by a semantic `HarnessRevisionChanged` entry in the
    /// session tree, so an orphan catalog object cannot accidentally activate
    /// a candidate after a crash.
    pub(crate) fn catalog_json(&self) -> Result<JsonValue, HarnessLineageError> {
        Ok(JsonValue::object([
            ("schema_version", JsonValue::from(CATALOG_SCHEMA_VERSION)),
            (
                "trees",
                JsonValue::Array(self.trees.values().map(encode_tree).collect::<Vec<_>>()),
            ),
            (
                "snapshots",
                JsonValue::Array(
                    self.snapshots
                        .values()
                        .map(encode_snapshot)
                        .collect::<Vec<_>>(),
                ),
            ),
            (
                "revisions",
                JsonValue::Array(
                    self.revisions
                        .values()
                        .map(encode_revision)
                        .collect::<Vec<_>>(),
                ),
            ),
            (
                "candidates",
                JsonValue::Array(
                    self.candidates
                        .values()
                        .map(encode_candidate)
                        .collect::<Vec<_>>(),
                ),
            ),
        ]))
    }

    /// Rebuild a repository from a catalog object and exact immutable source
    /// artifacts.  This is deliberately stricter than ordinary loading:
    /// every tree object, source blob, snapshot fingerprint, candidate
    /// validation result, and canonical ID is recomputed before the caller can
    /// resolve an active revision.
    pub(crate) fn from_catalog_json(
        artifacts: Arc<dyn ArtifactStore>,
        value: &JsonValue,
    ) -> Result<Self, HarnessLineageError> {
        let object = required_object(
            value,
            &[
                "schema_version",
                "trees",
                "snapshots",
                "revisions",
                "candidates",
            ],
        )?;
        if required_u64(object, "schema_version")? != CATALOG_SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported harness catalog schema version {}; expected {CATALOG_SCHEMA_VERSION}",
                required_u64(object, "schema_version")?
            )));
        }

        let mut repository = Self::new(artifacts);
        for value in required_array(object, "trees")? {
            let tree = decode_tree(value)?;
            if tree.files.is_empty() {
                return Err(invalid(format!("catalog tree {} has no files", tree.id)));
            }
            let calculated = tree_id(&tree.files)?;
            if calculated != tree.id {
                return Err(invalid(format!(
                    "catalog tree {} does not match its canonical file metadata digest",
                    tree.id
                )));
            }
            for file in tree.files.values() {
                let _ = load_tree_file(repository.artifacts.as_ref(), file)?;
            }
            match repository.trees.get(&tree.id) {
                Some(existing) if existing != &tree => {
                    return Err(invalid(format!(
                        "catalog materializes tree {} with conflicting immutable metadata",
                        tree.id
                    )));
                }
                Some(_) => {}
                None => {
                    repository.trees.insert(tree.id.clone(), tree);
                }
            }
        }

        for value in required_array(object, "snapshots")? {
            let expected = decode_snapshot(value)?;
            let staged = repository.stage_snapshot(expected.spec.clone())?;
            if staged != expected {
                return Err(invalid(format!(
                    "catalog snapshot {} does not match source-derived canonical snapshot data",
                    expected.id
                )));
            }
        }

        let mut pending_revisions = required_array(object, "revisions")?
            .iter()
            .map(decode_revision)
            .collect::<Result<Vec<_>, _>>()?;
        while !pending_revisions.is_empty() {
            let before = pending_revisions.len();
            let mut next = Vec::new();
            for revision in pending_revisions {
                if !repository.snapshots.contains_key(&revision.snapshot_id) {
                    return Err(invalid(format!(
                        "catalog revision {} references missing snapshot {}",
                        revision.revision_id, revision.snapshot_id
                    )));
                }
                if revision
                    .parent_revision_ids
                    .iter()
                    .all(|parent| repository.revisions.contains_key(parent))
                {
                    let calculated = revision_id(
                        &revision.snapshot_id,
                        &revision.parent_revision_ids,
                        revision.actor,
                        &revision.reason,
                        revision.candidate_id.as_ref(),
                    )?;
                    if calculated != revision.revision_id {
                        return Err(invalid(format!(
                            "catalog revision {} does not match its canonical identity",
                            revision.revision_id
                        )));
                    }
                    match repository.revisions.get(&revision.revision_id) {
                        Some(existing) if existing != &revision => {
                            return Err(invalid(format!(
                                "catalog revision {} has conflicting immutable metadata",
                                revision.revision_id
                            )));
                        }
                        Some(_) => {}
                        None => {
                            repository
                                .revisions
                                .insert(revision.revision_id.clone(), revision);
                        }
                    }
                } else {
                    next.push(revision);
                }
            }
            if next.len() == before {
                let unresolved = next
                    .iter()
                    .map(|revision| revision.revision_id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(invalid(format!(
                    "catalog revisions have a missing or cyclic parent chain: {unresolved}"
                )));
            }
            pending_revisions = next;
        }

        for value in required_array(object, "candidates")? {
            let candidate = decode_candidate(value)?;
            let parent = repository
                .revisions
                .get(&candidate.draft.parent_revision_id)
                .ok_or_else(|| {
                    invalid(format!(
                        "catalog candidate {} references missing parent revision {}",
                        candidate.candidate_id, candidate.draft.parent_revision_id
                    ))
                })?;
            let snapshot = repository
                .snapshots
                .get(&candidate.draft.proposed_snapshot_id)
                .ok_or_else(|| {
                    invalid(format!(
                        "catalog candidate {} references missing snapshot {}",
                        candidate.candidate_id, candidate.draft.proposed_snapshot_id
                    ))
                })?;
            let calculated_id = candidate_id(&candidate.draft)?;
            if calculated_id != candidate.candidate_id {
                return Err(invalid(format!(
                    "catalog candidate {} does not match its canonical identity",
                    candidate.candidate_id
                )));
            }
            let calculated_validation =
                validate_candidate(&candidate.draft, parent, snapshot, &repository.trees)?;
            if calculated_validation != candidate.validation {
                return Err(invalid(format!(
                    "catalog candidate {} does not match deterministic validation evidence",
                    candidate.candidate_id
                )));
            }
            match repository.candidates.get(&candidate.candidate_id) {
                Some(existing) if existing != &candidate => {
                    return Err(invalid(format!(
                        "catalog candidate {} has conflicting immutable metadata",
                        candidate.candidate_id
                    )));
                }
                Some(_) => {}
                None => {
                    repository
                        .candidates
                        .insert(candidate.candidate_id.clone(), candidate);
                }
            }
        }

        Ok(repository)
    }
}

fn encode_tree(tree: &HarnessTree) -> JsonValue {
    JsonValue::object([
        ("id", string(&tree.id)),
        (
            "files",
            JsonValue::Array(
                tree.files
                    .values()
                    .map(|file| {
                        JsonValue::object([
                            ("path", JsonValue::String(file.path.to_string())),
                            ("artifact_id", JsonValue::String(file.artifact_id.to_hex())),
                            ("byte_len", JsonValue::from(file.byte_len)),
                            ("media_type", JsonValue::String(file.media_type.clone())),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn decode_tree(value: &JsonValue) -> Result<HarnessTree, HarnessLineageError> {
    let object = required_object(value, &["id", "files"])?;
    let id = parse_tree_id(required_string(object, "id")?)?;
    let mut files = BTreeMap::new();
    for value in required_array(object, "files")? {
        let file = required_object(value, &["path", "artifact_id", "byte_len", "media_type"])?;
        let path = NormalizedPath::new(required_string(file, "path")?)
            .map_err(|error| invalid(error.to_string()))?;
        let next = HarnessTreeFile {
            path: path.clone(),
            artifact_id: ArtifactId::from_hex(required_string(file, "artifact_id")?)
                .map_err(|error| invalid(error.to_string()))?,
            byte_len: required_u64(file, "byte_len")?,
            media_type: required_string(file, "media_type")?.to_owned(),
        };
        if files.insert(path.clone(), next).is_some() {
            return Err(invalid(format!(
                "catalog tree {id} repeats source path {path}"
            )));
        }
    }
    Ok(HarnessTree { id, files })
}

fn encode_snapshot(snapshot: &HarnessSnapshotV1) -> JsonValue {
    JsonValue::object([
        ("id", string(&snapshot.id)),
        (
            "schema_version",
            JsonValue::from(u64::from(snapshot.schema_version)),
        ),
        (
            "luau_abi_version",
            JsonValue::from(u64::from(snapshot.luau_abi_version)),
        ),
        ("spec", encode_snapshot_spec(&snapshot.spec)),
        ("fingerprints", encode_fingerprints(&snapshot.fingerprints)),
    ])
}

fn decode_snapshot(value: &JsonValue) -> Result<HarnessSnapshotV1, HarnessLineageError> {
    let object = required_object(
        value,
        &[
            "id",
            "schema_version",
            "luau_abi_version",
            "spec",
            "fingerprints",
        ],
    )?;
    Ok(HarnessSnapshotV1 {
        id: parse_snapshot_id(required_string(object, "id")?)?,
        schema_version: parse_u16(
            required_u64(object, "schema_version")?,
            "snapshot schema version",
        )?,
        luau_abi_version: parse_u16(
            required_u64(object, "luau_abi_version")?,
            "Luau ABI version",
        )?,
        spec: decode_snapshot_spec(required_value(object, "spec")?)?,
        fingerprints: decode_fingerprints(required_value(object, "fingerprints")?)?,
    })
}

fn encode_snapshot_spec(spec: &HarnessSnapshotSpec) -> JsonValue {
    JsonValue::object([
        (
            "base_profile_digest",
            JsonValue::String(spec.base_profile_digest.to_hex()),
        ),
        (
            "base_system_prompt",
            JsonValue::String(spec.base_system_prompt.clone()),
        ),
        ("model_harness_profile", string(&spec.model_harness_profile)),
        (
            "self_extension_addendum",
            optional_string(spec.self_extension_addendum.as_deref()),
        ),
        (
            "ordered_global_plugins",
            JsonValue::Array(
                spec.ordered_global_plugins
                    .iter()
                    .map(encode_bundle)
                    .collect(),
            ),
        ),
        (
            "ordered_session_plugins",
            JsonValue::Array(
                spec.ordered_session_plugins
                    .iter()
                    .map(encode_bundle)
                    .collect(),
            ),
        ),
        (
            "prompt_sections",
            JsonValue::Array(
                spec.prompt_sections
                    .iter()
                    .map(encode_prompt_section)
                    .collect(),
            ),
        ),
        (
            "plugin_prompt_sections",
            JsonValue::Array(
                spec.plugin_prompt_sections
                    .iter()
                    .map(encode_prompt_section)
                    .collect(),
            ),
        ),
        (
            "tool_presentations",
            JsonValue::Array(
                spec.tool_presentations
                    .iter()
                    .map(encode_tool_presentation)
                    .collect(),
            ),
        ),
        (
            "plugin_tool_presentations",
            JsonValue::Array(
                spec.plugin_tool_presentations
                    .iter()
                    .map(encode_tool_presentation)
                    .collect(),
            ),
        ),
        (
            "hook_bundle_digest",
            JsonValue::String(spec.hook_bundle_digest.to_hex()),
        ),
        (
            "capability_bindings",
            JsonValue::Array(
                spec.capability_bindings
                    .iter()
                    .map(encode_capability_binding)
                    .collect(),
            ),
        ),
        (
            "resource_limits",
            encode_resource_limits(&spec.resource_limits),
        ),
        (
            "compaction_policy_digest",
            JsonValue::String(spec.compaction_policy_digest.to_hex()),
        ),
        (
            "tool_projection_digest",
            JsonValue::String(spec.tool_projection_digest.to_hex()),
        ),
        (
            "failure_policy_digest",
            JsonValue::String(spec.failure_policy_digest.to_hex()),
        ),
    ])
}

fn decode_snapshot_spec(value: &JsonValue) -> Result<HarnessSnapshotSpec, HarnessLineageError> {
    let object = required_object(
        value,
        &[
            "base_profile_digest",
            "base_system_prompt",
            "model_harness_profile",
            "self_extension_addendum",
            "ordered_global_plugins",
            "ordered_session_plugins",
            "prompt_sections",
            "plugin_prompt_sections",
            "tool_presentations",
            "plugin_tool_presentations",
            "hook_bundle_digest",
            "capability_bindings",
            "resource_limits",
            "compaction_policy_digest",
            "tool_projection_digest",
            "failure_policy_digest",
        ],
    )?;
    Ok(HarnessSnapshotSpec {
        base_profile_digest: parse_digest(required_string(object, "base_profile_digest")?)?,
        base_system_prompt: required_string(object, "base_system_prompt")?.to_owned(),
        model_harness_profile: parse_profile_id(required_string(object, "model_harness_profile")?)?,
        self_extension_addendum: parse_optional_string(required_value(
            object,
            "self_extension_addendum",
        )?)?,
        ordered_global_plugins: required_array(object, "ordered_global_plugins")?
            .iter()
            .map(decode_bundle)
            .collect::<Result<Vec<_>, _>>()?,
        ordered_session_plugins: required_array(object, "ordered_session_plugins")?
            .iter()
            .map(decode_bundle)
            .collect::<Result<Vec<_>, _>>()?,
        prompt_sections: required_array(object, "prompt_sections")?
            .iter()
            .map(decode_prompt_section)
            .collect::<Result<Vec<_>, _>>()?,
        plugin_prompt_sections: required_array(object, "plugin_prompt_sections")?
            .iter()
            .map(decode_prompt_section)
            .collect::<Result<Vec<_>, _>>()?,
        tool_presentations: required_array(object, "tool_presentations")?
            .iter()
            .map(decode_tool_presentation)
            .collect::<Result<Vec<_>, _>>()?,
        plugin_tool_presentations: required_array(object, "plugin_tool_presentations")?
            .iter()
            .map(decode_tool_presentation)
            .collect::<Result<Vec<_>, _>>()?,
        hook_bundle_digest: parse_digest(required_string(object, "hook_bundle_digest")?)?,
        capability_bindings: required_array(object, "capability_bindings")?
            .iter()
            .map(decode_capability_binding)
            .collect::<Result<Vec<_>, _>>()?,
        resource_limits: decode_resource_limits(required_value(object, "resource_limits")?)?,
        compaction_policy_digest: parse_digest(required_string(
            object,
            "compaction_policy_digest",
        )?)?,
        tool_projection_digest: parse_digest(required_string(object, "tool_projection_digest")?)?,
        failure_policy_digest: parse_digest(required_string(object, "failure_policy_digest")?)?,
    })
}

fn encode_bundle(bundle: &PluginBundleRef) -> JsonValue {
    JsonValue::object([
        ("plugin_id", JsonValue::String(bundle.plugin_id.clone())),
        ("tree_id", string(&bundle.tree_id)),
        (
            "requested_capabilities",
            JsonValue::Array(
                bundle
                    .requested_capabilities
                    .iter()
                    .cloned()
                    .map(JsonValue::String)
                    .collect(),
            ),
        ),
    ])
}

fn decode_bundle(value: &JsonValue) -> Result<PluginBundleRef, HarnessLineageError> {
    let object = required_object(value, &["plugin_id", "tree_id", "requested_capabilities"])?;
    let mut requested_capabilities = BTreeSet::new();
    for value in required_array(object, "requested_capabilities")? {
        let capability = value
            .as_str()
            .ok_or_else(|| invalid("catalog capability names must be strings"))?
            .to_owned();
        if !requested_capabilities.insert(capability.clone()) {
            return Err(invalid(format!(
                "catalog bundle repeats requested capability {capability}"
            )));
        }
    }
    Ok(PluginBundleRef {
        plugin_id: required_string(object, "plugin_id")?.to_owned(),
        tree_id: parse_tree_id(required_string(object, "tree_id")?)?,
        requested_capabilities,
    })
}

fn encode_prompt_section(section: &PromptSectionDescriptor) -> JsonValue {
    JsonValue::object([
        ("id", JsonValue::String(section.id.clone())),
        ("content", JsonValue::String(section.content.clone())),
    ])
}

fn decode_prompt_section(
    value: &JsonValue,
) -> Result<PromptSectionDescriptor, HarnessLineageError> {
    let object = required_object(value, &["id", "content"])?;
    Ok(PromptSectionDescriptor {
        id: required_string(object, "id")?.to_owned(),
        content: required_string(object, "content")?.to_owned(),
    })
}

fn encode_tool_presentation(tool: &ToolPresentationDescriptor) -> JsonValue {
    JsonValue::object([
        ("name", JsonValue::String(tool.name.clone())),
        ("description", JsonValue::String(tool.description.clone())),
        ("schema", tool.schema.clone()),
        (
            "execution_mode",
            JsonValue::String(tool.execution_mode.clone()),
        ),
    ])
}

fn decode_tool_presentation(
    value: &JsonValue,
) -> Result<ToolPresentationDescriptor, HarnessLineageError> {
    let object = required_object(value, &["name", "description", "schema", "execution_mode"])?;
    Ok(ToolPresentationDescriptor {
        name: required_string(object, "name")?.to_owned(),
        description: required_string(object, "description")?.to_owned(),
        schema: required_value(object, "schema")?.clone(),
        execution_mode: required_string(object, "execution_mode")?.to_owned(),
    })
}

fn encode_capability_binding(binding: &CapabilityBindingRef) -> JsonValue {
    JsonValue::object([
        ("plugin_id", JsonValue::String(binding.plugin_id.clone())),
        ("capability", JsonValue::String(binding.capability.clone())),
        (
            "capability_version",
            JsonValue::String(binding.capability_version.clone()),
        ),
        (
            "binding_digest",
            JsonValue::String(binding.binding_digest.to_hex()),
        ),
    ])
}

fn decode_capability_binding(
    value: &JsonValue,
) -> Result<CapabilityBindingRef, HarnessLineageError> {
    let object = required_object(
        value,
        &[
            "plugin_id",
            "capability",
            "capability_version",
            "binding_digest",
        ],
    )?;
    Ok(CapabilityBindingRef {
        plugin_id: required_string(object, "plugin_id")?.to_owned(),
        capability: required_string(object, "capability")?.to_owned(),
        capability_version: required_string(object, "capability_version")?.to_owned(),
        binding_digest: parse_digest(required_string(object, "binding_digest")?)?,
    })
}

fn encode_resource_limits(limits: &HarnessResourceLimits) -> JsonValue {
    JsonValue::object([
        ("source_bytes", JsonValue::from(limits.source_bytes as u64)),
        ("memory_bytes", JsonValue::from(limits.memory_bytes as u64)),
        (
            "instruction_checks",
            JsonValue::from(u64::from(limits.instruction_checks)),
        ),
        (
            "provider_surface_bytes",
            JsonValue::from(limits.provider_surface_bytes as u64),
        ),
    ])
}

fn decode_resource_limits(value: &JsonValue) -> Result<HarnessResourceLimits, HarnessLineageError> {
    let object = required_object(
        value,
        &[
            "source_bytes",
            "memory_bytes",
            "instruction_checks",
            "provider_surface_bytes",
        ],
    )?;
    Ok(HarnessResourceLimits {
        source_bytes: parse_usize(required_u64(object, "source_bytes")?, "source byte limit")?,
        memory_bytes: parse_usize(required_u64(object, "memory_bytes")?, "memory byte limit")?,
        instruction_checks: u32::try_from(required_u64(object, "instruction_checks")?)
            .map_err(|_| invalid("instruction check limit exceeds u32"))?,
        provider_surface_bytes: parse_usize(
            required_u64(object, "provider_surface_bytes")?,
            "provider surface byte limit",
        )?,
    })
}

fn encode_fingerprints(value: &HarnessSurfaceFingerprints) -> JsonValue {
    JsonValue::object([
        (
            "system_prompt_digest",
            JsonValue::String(value.system_prompt_digest.to_hex()),
        ),
        (
            "ordered_tool_definitions_digest",
            JsonValue::String(value.ordered_tool_definitions_digest.to_hex()),
        ),
        (
            "hook_bundle_digest",
            JsonValue::String(value.hook_bundle_digest.to_hex()),
        ),
        (
            "capability_bindings_digest",
            JsonValue::String(value.capability_bindings_digest.to_hex()),
        ),
        (
            "compaction_policy_digest",
            JsonValue::String(value.compaction_policy_digest.to_hex()),
        ),
        (
            "provider_surface_digest",
            JsonValue::String(value.provider_surface_digest.to_hex()),
        ),
    ])
}

fn decode_fingerprints(
    value: &JsonValue,
) -> Result<HarnessSurfaceFingerprints, HarnessLineageError> {
    let object = required_object(
        value,
        &[
            "system_prompt_digest",
            "ordered_tool_definitions_digest",
            "hook_bundle_digest",
            "capability_bindings_digest",
            "compaction_policy_digest",
            "provider_surface_digest",
        ],
    )?;
    Ok(HarnessSurfaceFingerprints {
        system_prompt_digest: parse_digest(required_string(object, "system_prompt_digest")?)?,
        ordered_tool_definitions_digest: parse_digest(required_string(
            object,
            "ordered_tool_definitions_digest",
        )?)?,
        hook_bundle_digest: parse_digest(required_string(object, "hook_bundle_digest")?)?,
        capability_bindings_digest: parse_digest(required_string(
            object,
            "capability_bindings_digest",
        )?)?,
        compaction_policy_digest: parse_digest(required_string(
            object,
            "compaction_policy_digest",
        )?)?,
        provider_surface_digest: parse_digest(required_string(object, "provider_surface_digest")?)?,
    })
}

fn encode_revision(revision: &HarnessRevisionV1) -> JsonValue {
    JsonValue::object([
        ("revision_id", string(&revision.revision_id)),
        ("snapshot_id", string(&revision.snapshot_id)),
        (
            "parent_revision_ids",
            JsonValue::Array(
                revision
                    .parent_revision_ids
                    .iter()
                    .map(string)
                    .collect(),
            ),
        ),
        (
            "actor",
            JsonValue::String(encode_actor(revision.actor).into()),
        ),
        (
            "reason",
            JsonValue::String(encode_revision_reason(&revision.reason).into()),
        ),
        (
            "candidate_id",
            revision
                .candidate_id
                .as_ref()
                .map(string)
                .unwrap_or(JsonValue::Null),
        ),
        ("created_at_ms", JsonValue::from(revision.created_at_ms)),
    ])
}

fn decode_revision(value: &JsonValue) -> Result<HarnessRevisionV1, HarnessLineageError> {
    let object = required_object(
        value,
        &[
            "revision_id",
            "snapshot_id",
            "parent_revision_ids",
            "actor",
            "reason",
            "candidate_id",
            "created_at_ms",
        ],
    )?;
    Ok(HarnessRevisionV1 {
        revision_id: parse_revision_id(required_string(object, "revision_id")?)?,
        snapshot_id: parse_snapshot_id(required_string(object, "snapshot_id")?)?,
        parent_revision_ids: required_array(object, "parent_revision_ids")?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| invalid("catalog revision parent IDs must be strings"))
                    .and_then(parse_revision_id)
            })
            .collect::<Result<Vec<_>, _>>()?,
        actor: parse_actor(required_string(object, "actor")?)?,
        reason: parse_revision_reason(required_string(object, "reason")?)?,
        candidate_id: parse_optional_id(
            required_value(object, "candidate_id")?,
            parse_candidate_id,
        )?,
        created_at_ms: required_u64(object, "created_at_ms")?,
    })
}

fn encode_candidate(candidate: &HarnessCandidateV1) -> JsonValue {
    JsonValue::object([
        ("candidate_id", string(&candidate.candidate_id)),
        ("draft", encode_candidate_draft(&candidate.draft)),
        (
            "validation",
            encode_candidate_validation(&candidate.validation),
        ),
    ])
}

fn decode_candidate(value: &JsonValue) -> Result<HarnessCandidateV1, HarnessLineageError> {
    let object = required_object(value, &["candidate_id", "draft", "validation"])?;
    Ok(HarnessCandidateV1 {
        candidate_id: parse_candidate_id(required_string(object, "candidate_id")?)?,
        draft: decode_candidate_draft(required_value(object, "draft")?)?,
        validation: decode_candidate_validation(required_value(object, "validation")?)?,
    })
}

fn encode_candidate_draft(draft: &HarnessCandidateDraft) -> JsonValue {
    JsonValue::object([
        ("parent_revision_id", string(&draft.parent_revision_id)),
        ("proposed_snapshot_id", string(&draft.proposed_snapshot_id)),
        ("actor", JsonValue::String(encode_actor(draft.actor).into())),
        (
            "operation_id",
            draft
                .operation_id
                .as_ref()
                .map(string)
                .unwrap_or(JsonValue::Null),
        ),
        (
            "tool_invocation_id",
            optional_string(draft.tool_invocation_id.as_deref()),
        ),
        (
            "hypothesis",
            JsonValue::object([
                (
                    "targeted_evidence",
                    JsonValue::String(draft.hypothesis.targeted_evidence.clone()),
                ),
                (
                    "expected_effect",
                    JsonValue::String(draft.hypothesis.expected_effect.clone()),
                ),
                (
                    "regression_risk",
                    JsonValue::String(draft.hypothesis.regression_risk.clone()),
                ),
            ]),
        ),
        (
            "changed_paths",
            JsonValue::Array(
                draft
                    .changed_paths
                    .iter()
                    .map(|path| JsonValue::String(path.to_string()))
                    .collect(),
            ),
        ),
        (
            "registry_operations",
            JsonValue::Array(
                draft
                    .registry_operations
                    .iter()
                    .map(encode_registry_operation)
                    .collect(),
            ),
        ),
        (
            "changed_surfaces",
            JsonValue::Array(
                draft
                    .changed_surfaces
                    .iter()
                    .map(|surface| JsonValue::String(encode_surface(*surface).into()))
                    .collect(),
            ),
        ),
        ("targeted_failures", string_array(&draft.targeted_failures)),
        ("evidence", string_array(&draft.evidence)),
        ("expected_effects", string_array(&draft.expected_effects)),
        ("regression_risks", string_array(&draft.regression_risks)),
        (
            "capability_ceiling",
            JsonValue::Array(
                draft
                    .capability_ceiling
                    .iter()
                    .cloned()
                    .map(JsonValue::String)
                    .collect(),
            ),
        ),
    ])
}

fn decode_candidate_draft(value: &JsonValue) -> Result<HarnessCandidateDraft, HarnessLineageError> {
    let object = required_object(
        value,
        &[
            "parent_revision_id",
            "proposed_snapshot_id",
            "actor",
            "operation_id",
            "tool_invocation_id",
            "hypothesis",
            "changed_paths",
            "registry_operations",
            "changed_surfaces",
            "targeted_failures",
            "evidence",
            "expected_effects",
            "regression_risks",
            "capability_ceiling",
        ],
    )?;
    let hypothesis = required_object(
        required_value(object, "hypothesis")?,
        &["targeted_evidence", "expected_effect", "regression_risk"],
    )?;
    let mut changed_surfaces = BTreeSet::new();
    for value in required_array(object, "changed_surfaces")? {
        let surface = value
            .as_str()
            .ok_or_else(|| invalid("catalog changed surfaces must be strings"))
            .and_then(parse_surface)?;
        if !changed_surfaces.insert(surface) {
            return Err(invalid("catalog candidate repeats a changed surface"));
        }
    }
    let mut capability_ceiling = BTreeSet::new();
    for value in required_array(object, "capability_ceiling")? {
        let capability = value
            .as_str()
            .ok_or_else(|| invalid("catalog capability ceiling names must be strings"))?
            .to_owned();
        if !capability_ceiling.insert(capability) {
            return Err(invalid(
                "catalog candidate repeats a capability ceiling name",
            ));
        }
    }
    Ok(HarnessCandidateDraft {
        parent_revision_id: parse_revision_id(required_string(object, "parent_revision_id")?)?,
        proposed_snapshot_id: parse_snapshot_id(required_string(object, "proposed_snapshot_id")?)?,
        actor: parse_actor(required_string(object, "actor")?)?,
        operation_id: parse_optional_id(
            required_value(object, "operation_id")?,
            parse_operation_id,
        )?,
        tool_invocation_id: parse_optional_string(required_value(object, "tool_invocation_id")?)?,
        hypothesis: CandidateHypothesis {
            targeted_evidence: required_string(hypothesis, "targeted_evidence")?.to_owned(),
            expected_effect: required_string(hypothesis, "expected_effect")?.to_owned(),
            regression_risk: required_string(hypothesis, "regression_risk")?.to_owned(),
        },
        changed_paths: required_array(object, "changed_paths")?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| invalid("catalog changed paths must be strings"))
                    .and_then(|path| {
                        NormalizedPath::new(path).map_err(|error| invalid(error.to_string()))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?,
        registry_operations: required_array(object, "registry_operations")?
            .iter()
            .map(decode_registry_operation)
            .collect::<Result<Vec<_>, _>>()?,
        changed_surfaces,
        targeted_failures: decode_string_array(
            required_array(object, "targeted_failures")?,
            "targeted failures",
        )?,
        evidence: decode_string_array(required_array(object, "evidence")?, "evidence")?,
        expected_effects: decode_string_array(
            required_array(object, "expected_effects")?,
            "expected effects",
        )?,
        regression_risks: decode_string_array(
            required_array(object, "regression_risks")?,
            "regression risks",
        )?,
        capability_ceiling,
    })
}

fn encode_candidate_validation(validation: &CandidateValidation) -> JsonValue {
    JsonValue::object([
        ("accepted", JsonValue::Bool(validation.accepted)),
        ("is_noop", JsonValue::Bool(validation.is_noop)),
        ("diagnostics", string_array(&validation.diagnostics)),
    ])
}

fn decode_candidate_validation(
    value: &JsonValue,
) -> Result<CandidateValidation, HarnessLineageError> {
    let object = required_object(value, &["accepted", "is_noop", "diagnostics"])?;
    Ok(CandidateValidation {
        accepted: required_bool(object, "accepted")?,
        is_noop: required_bool(object, "is_noop")?,
        diagnostics: decode_string_array(required_array(object, "diagnostics")?, "diagnostics")?,
    })
}

fn encode_registry_operation(operation: &RegistryOperation) -> JsonValue {
    match operation {
        RegistryOperation::Add { plugin_id } => JsonValue::object([
            ("operation", JsonValue::String("add".into())),
            ("plugin_id", JsonValue::String(plugin_id.clone())),
        ]),
        RegistryOperation::Remove { plugin_id } => JsonValue::object([
            ("operation", JsonValue::String("remove".into())),
            ("plugin_id", JsonValue::String(plugin_id.clone())),
        ]),
    }
}

fn decode_registry_operation(value: &JsonValue) -> Result<RegistryOperation, HarnessLineageError> {
    let object = required_object(value, &["operation", "plugin_id"])?;
    let plugin_id = required_string(object, "plugin_id")?.to_owned();
    match required_string(object, "operation")? {
        "add" => Ok(RegistryOperation::Add { plugin_id }),
        "remove" => Ok(RegistryOperation::Remove { plugin_id }),
        other => Err(invalid(format!(
            "catalog registry operation {other:?} is unknown"
        ))),
    }
}

fn encode_actor(actor: HarnessActor) -> &'static str {
    match actor {
        HarnessActor::Host => "host",
        HarnessActor::Operator => "operator",
        HarnessActor::Model => "model",
    }
}

fn parse_actor(value: &str) -> Result<HarnessActor, HarnessLineageError> {
    match value {
        "host" => Ok(HarnessActor::Host),
        "operator" => Ok(HarnessActor::Operator),
        "model" => Ok(HarnessActor::Model),
        _ => Err(invalid(format!("unknown catalog harness actor {value:?}"))),
    }
}

fn encode_revision_reason(reason: &HarnessRevisionReason) -> &'static str {
    match reason {
        HarnessRevisionReason::Initial => "initial",
        HarnessRevisionReason::CandidateActivation => "candidate_activation",
        HarnessRevisionReason::GlobalRebase => "global_rebase",
        HarnessRevisionReason::Rollback => "rollback",
    }
}

fn parse_revision_reason(value: &str) -> Result<HarnessRevisionReason, HarnessLineageError> {
    match value {
        "initial" => Ok(HarnessRevisionReason::Initial),
        "candidate_activation" => Ok(HarnessRevisionReason::CandidateActivation),
        "global_rebase" => Ok(HarnessRevisionReason::GlobalRebase),
        "rollback" => Ok(HarnessRevisionReason::Rollback),
        _ => Err(invalid(format!(
            "unknown catalog revision reason {value:?}"
        ))),
    }
}

fn encode_surface(surface: HarnessSurface) -> &'static str {
    match surface {
        HarnessSurface::SystemPrompt => "system_prompt",
        HarnessSurface::ToolDefinitions => "tool_definitions",
        HarnessSurface::Hooks => "hooks",
        HarnessSurface::CapabilityBindings => "capability_bindings",
        HarnessSurface::Compaction => "compaction",
        HarnessSurface::ToolProjection => "tool_projection",
        HarnessSurface::FailurePolicy => "failure_policy",
    }
}

fn parse_surface(value: &str) -> Result<HarnessSurface, HarnessLineageError> {
    match value {
        "system_prompt" => Ok(HarnessSurface::SystemPrompt),
        "tool_definitions" => Ok(HarnessSurface::ToolDefinitions),
        "hooks" => Ok(HarnessSurface::Hooks),
        "capability_bindings" => Ok(HarnessSurface::CapabilityBindings),
        "compaction" => Ok(HarnessSurface::Compaction),
        "tool_projection" => Ok(HarnessSurface::ToolProjection),
        "failure_policy" => Ok(HarnessSurface::FailurePolicy),
        _ => Err(invalid(format!(
            "unknown catalog harness surface {value:?}"
        ))),
    }
}

fn required_object<'a>(
    value: &'a JsonValue,
    fields: &[&str],
) -> Result<&'a BTreeMap<String, JsonValue>, HarnessLineageError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("catalog value must be a JSON object"))?;
    for field in fields {
        if !object.contains_key(*field) {
            return Err(invalid(format!(
                "catalog object is missing required field {field}"
            )));
        }
    }
    for field in object.keys() {
        if !fields.contains(&field.as_str()) {
            return Err(invalid(format!("catalog object has unknown field {field}")));
        }
    }
    Ok(object)
}

fn required_value<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<&'a JsonValue, HarnessLineageError> {
    object
        .get(field)
        .ok_or_else(|| invalid(format!("catalog object is missing required field {field}")))
}

fn required_array<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<&'a [JsonValue], HarnessLineageError> {
    required_value(object, field)?
        .as_array()
        .ok_or_else(|| invalid(format!("catalog field {field} must be an array")))
}

fn required_string<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<&'a str, HarnessLineageError> {
    required_value(object, field)?
        .as_str()
        .ok_or_else(|| invalid(format!("catalog field {field} must be a string")))
}

fn required_u64(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<u64, HarnessLineageError> {
    required_value(object, field)?
        .as_u64()
        .ok_or_else(|| invalid(format!("catalog field {field} must be an unsigned integer")))
}

fn required_bool(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<bool, HarnessLineageError> {
    required_value(object, field)?
        .as_bool()
        .ok_or_else(|| invalid(format!("catalog field {field} must be a boolean")))
}

fn parse_optional_string(value: &JsonValue) -> Result<Option<String>, HarnessLineageError> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::String(value) => Ok(Some(value.clone())),
        _ => Err(invalid("catalog optional string must be a string or null")),
    }
}

fn parse_optional_id<T>(
    value: &JsonValue,
    parse: impl FnOnce(&str) -> Result<T, HarnessLineageError>,
) -> Result<Option<T>, HarnessLineageError> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::String(value) => parse(value).map(Some),
        _ => Err(invalid("catalog optional ID must be a string or null")),
    }
}

fn parse_digest(value: &str) -> Result<Digest, HarnessLineageError> {
    Digest::from_hex(value).map_err(|error| invalid(error.to_string()))
}

fn parse_tree_id(value: &str) -> Result<HarnessTreeId, HarnessLineageError> {
    HarnessTreeId::new(value.to_owned()).map_err(|error| invalid(error.to_string()))
}

fn parse_snapshot_id(value: &str) -> Result<HarnessSnapshotId, HarnessLineageError> {
    HarnessSnapshotId::new(value.to_owned()).map_err(|error| invalid(error.to_string()))
}

fn parse_revision_id(value: &str) -> Result<HarnessRevisionId, HarnessLineageError> {
    HarnessRevisionId::new(value.to_owned()).map_err(|error| invalid(error.to_string()))
}

fn parse_candidate_id(value: &str) -> Result<HarnessCandidateId, HarnessLineageError> {
    HarnessCandidateId::new(value.to_owned()).map_err(|error| invalid(error.to_string()))
}

fn parse_profile_id(value: &str) -> Result<ModelHarnessProfileId, HarnessLineageError> {
    ModelHarnessProfileId::new(value.to_owned()).map_err(|error| invalid(error.to_string()))
}

fn parse_operation_id(value: &str) -> Result<OperationId, HarnessLineageError> {
    OperationId::new(value.to_owned()).map_err(|error| invalid(error.to_string()))
}

fn parse_u16(value: u64, label: &str) -> Result<u16, HarnessLineageError> {
    u16::try_from(value).map_err(|_| invalid(format!("catalog {label} exceeds u16")))
}

fn parse_usize(value: u64, label: &str) -> Result<usize, HarnessLineageError> {
    usize::try_from(value).map_err(|_| invalid(format!("catalog {label} exceeds platform bounds")))
}

fn decode_string_array(
    values: &[JsonValue],
    label: &str,
) -> Result<Vec<String>, HarnessLineageError> {
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("catalog {label} must contain only strings")))
        })
        .collect()
}

fn string_array(values: &[String]) -> JsonValue {
    JsonValue::Array(values.iter().cloned().map(JsonValue::String).collect())
}

fn string<T: std::fmt::Display>(value: &T) -> JsonValue {
    JsonValue::String(value.to_string())
}

fn optional_string(value: Option<&str>) -> JsonValue {
    value
        .map(|value| JsonValue::String(value.to_owned()))
        .unwrap_or(JsonValue::Null)
}

fn invalid(message: impl Into<String>) -> HarnessLineageError {
    HarnessLineageError::Invalid {
        message: message.into(),
    }
}
