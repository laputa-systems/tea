//! Luau implementation of the core-owned immutable extension boundary.

use crate::bundle::{Bundle, BundleManifest, BUNDLE_ABI_VERSION};
use crate::tool_handler::{
    CapabilityBindings, CapabilityError, CapabilityFuture, CapabilityRequest, CapabilityResponse,
    HandlerLimits, LuaToolHandler, LuauCapability, ToolHandlerSpec,
};
use crate::{LuaPolicy, LuaPolicyHookSet, PolicyContextInput, PolicyLimits};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tea_core::harness::extension::{
    CollectedExtensionMemoryProposal, ExtensionCapability, ExtensionCapabilityBindings,
    ExtensionCapabilityError, ExtensionCapabilityRequest, ExtensionContextInput,
    ExtensionContextPatch, ExtensionContextPolicy, ExtensionDescriptor, ExtensionEngine,
    ExtensionError, ExtensionLifecycle, ExtensionLimits, ExtensionMemoryCollector,
    ExtensionPromptSection, ExtensionSourceTree, ExtensionToolDescription, ExtensionToolLimits,
    ResolvedExtension,
};
use tea_core::hooks::HookSet;
use tea_protocol::{JsonNumber, JsonValue};

/// The closed-bundle Luau implementation of [`ExtensionEngine`].
#[derive(Clone, Copy, Debug, Default)]
pub struct LuauExtensionEngine;

impl ExtensionEngine for LuauExtensionEngine {
    fn describe(
        &self,
        source: &ExtensionSourceTree,
    ) -> Result<ExtensionDescriptor, ExtensionError> {
        let (policy, requested_capabilities) = load_policy(source)?;
        descriptor(&policy, requested_capabilities)
    }

    fn resolve(
        &self,
        source: &ExtensionSourceTree,
        bindings: ExtensionCapabilityBindings,
        inner_hooks: Arc<dyn HookSet>,
        extension_index: usize,
        memory_collector: Arc<ExtensionMemoryCollector>,
    ) -> Result<ResolvedExtension, ExtensionError> {
        let (policy, _) = load_policy(source)?;
        let policy = Arc::new(policy);
        let mut tools = tea_core::tool::ToolRegistry::default();
        for tool in policy.tools() {
            let handler_source = tool.handler_source.clone().ok_or_else(|| {
                ExtensionError::new(format!(
                    "extension {} tool {} has no executable handler source",
                    source.extension_id, tool.name
                ))
            })?;
            let binding = bindings.get(&tool.capability).ok_or_else(|| {
                ExtensionError::new(format!(
                    "extension {} tool {} names unbound capability {}",
                    source.extension_id, tool.name, tool.capability
                ))
            })?;
            let limits = handler_limits(binding.limits());
            let handler = LuaToolHandler::new_with_limits(
                handler_source,
                ToolHandlerSpec {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    schema: tool.schema.clone(),
                    capability: tool.capability.clone(),
                    execution_mode: tool.execution_mode,
                    requires_exclusive_batch: tool.requires_exclusive_batch,
                    cancellation_settlement_mode: tool.cancellation_settlement_mode,
                },
                adapt_binding(&tool.capability, binding)?,
                limits,
            )
            .map_err(extension_error)?;
            tools.insert(Arc::new(handler));
        }
        let hooks = Arc::new(LuaPolicyHookSet::new_with_extension_memory(
            Arc::clone(&policy),
            source.extension_id.clone(),
            extension_index,
            memory_collector,
            inner_hooks,
        ));
        let context_policy = policy
            .has_context_projection()
            .map_err(extension_error)?
            .then(|| {
                Arc::new(LuauPolicyAdapter {
                    policy: Arc::clone(&policy),
                }) as Arc<dyn ExtensionContextPolicy>
            });
        let lifecycle = policy
            .has_resume_hooks()
            .map_err(extension_error)?
            .then(|| {
                Arc::new(LuauPolicyAdapter {
                    policy: Arc::clone(&policy),
                }) as Arc<dyn ExtensionLifecycle>
            });
        Ok(ResolvedExtension {
            hooks,
            tools,
            context_policy,
            lifecycle,
        })
    }
}

#[derive(Clone)]
struct LuauPolicyAdapter {
    policy: Arc<LuaPolicy>,
}

impl ExtensionContextPolicy for LuauPolicyAdapter {
    fn project_context(
        &self,
        input: &ExtensionContextInput,
    ) -> Result<ExtensionContextPatch, ExtensionError> {
        let proposal = self
            .policy
            .context_projection(&PolicyContextInput {
                entries: input
                    .entries
                    .iter()
                    .map(|entry| crate::PolicyContextEntry {
                        id: entry.id.clone(),
                        kind: entry.kind.clone(),
                        model_visible: entry.model_visible,
                        protected: entry.protected,
                    })
                    .collect(),
            })
            .map_err(extension_error)?;
        Ok(ExtensionContextPatch {
            retain_entries: proposal.retain_entries,
            omit_eligible_entries: proposal.omit_eligible_entries,
            annotations: proposal
                .annotations
                .into_iter()
                .map(
                    |annotation| tea_core::harness::extension::ExtensionContextAnnotation {
                        id: annotation.id,
                        content: annotation.content,
                    },
                )
                .collect(),
            selected_memory: proposal.selected_memory,
            requested_compaction_strategy: proposal.requested_compaction_strategy,
        })
    }
}

impl ExtensionLifecycle for LuauPolicyAdapter {
    fn hook_ids(&self) -> Result<Vec<String>, ExtensionError> {
        self.policy.resume_hook_ids().map_err(extension_error)
    }

    fn before_operation(&self) -> Result<BTreeMap<String, JsonValue>, ExtensionError> {
        self.policy
            .before_operation_resume_data()
            .map_err(extension_error)
    }

    fn before_epoch(&self) -> Result<BTreeMap<String, JsonValue>, ExtensionError> {
        self.policy
            .before_epoch_resume_data()
            .map_err(extension_error)
    }

    fn before_resume(
        &self,
        operation_data: &BTreeMap<String, JsonValue>,
        epoch_data: &BTreeMap<String, JsonValue>,
    ) -> Result<(), ExtensionError> {
        self.policy
            .before_resume(operation_data, epoch_data)
            .map_err(extension_error)
    }
}

fn descriptor(
    policy: &LuaPolicy,
    requested_capabilities: BTreeSet<String>,
) -> Result<ExtensionDescriptor, ExtensionError> {
    Ok(ExtensionDescriptor {
        requested_capabilities,
        prompt_sections: policy
            .prompt_sections()
            .iter()
            .map(|section| ExtensionPromptSection {
                id: section.id.clone(),
                content: section.content.clone(),
            })
            .collect(),
        tools: policy
            .tools()
            .iter()
            .map(|tool| ExtensionToolDescription {
                name: tool.name.clone(),
                description: tool.description.clone(),
                schema: tool.schema.clone(),
                capability: tool.capability.clone(),
                execution_mode: tool.execution_mode,
                requires_exclusive_batch: tool.requires_exclusive_batch,
                cancellation_settlement_mode: tool.cancellation_settlement_mode,
            })
            .collect(),
        lifecycle_hook_ids: policy.resume_hook_ids().map_err(extension_error)?,
    })
}

fn adapt_binding(
    capability: &str,
    binding: tea_core::harness::extension::ExtensionCapabilityBinding,
) -> Result<CapabilityBindings, ExtensionError> {
    let mut adapted = CapabilityBindings::new();
    adapted
        .insert(
            capability.to_owned(),
            Arc::new(CoreCapabilityAdapter {
                implementation: binding.implementation(),
            }),
        )
        .map_err(extension_error)?;
    Ok(adapted)
}

fn handler_limits(limits: ExtensionToolLimits) -> HandlerLimits {
    HandlerLimits {
        max_source_bytes: limits.max_source_bytes,
        max_memory_bytes: limits.max_memory_bytes,
        max_interrupt_checks: limits.max_interrupt_checks,
        max_capability_calls: limits.max_capability_calls,
    }
}

struct CoreCapabilityAdapter {
    implementation: Arc<dyn ExtensionCapability>,
}

impl LuauCapability for CoreCapabilityAdapter {
    fn invoke(
        &self,
        request: CapabilityRequest,
        cancellation: tea_core::scheduler::CancellationToken,
    ) -> CapabilityFuture {
        let future = self.implementation.invoke(
            ExtensionCapabilityRequest {
                call_id: request.call_id,
                tool_name: request.tool_name,
                capability: request.capability,
                method: request.method,
                arguments: request.arguments,
                updates: request.updates,
            },
            cancellation,
        );
        Box::pin(async move {
            future.await.map_or_else(
                |error| Err(map_capability_error(error)),
                |response| {
                    Ok(CapabilityResponse {
                        value: response.value,
                    })
                },
            )
        })
    }
}

fn map_capability_error(error: ExtensionCapabilityError) -> CapabilityError {
    match error {
        ExtensionCapabilityError::Cancelled => CapabilityError::Cancelled,
        ExtensionCapabilityError::NotBound { capability } => {
            CapabilityError::NotBound { capability }
        }
        ExtensionCapabilityError::MethodDenied { capability, method } => {
            CapabilityError::MethodDenied { capability, method }
        }
        ExtensionCapabilityError::InvalidArguments { message } => {
            CapabilityError::InvalidArguments { message }
        }
        ExtensionCapabilityError::Execution { message } => CapabilityError::Execution { message },
    }
}

fn load_policy(
    source: &ExtensionSourceTree,
) -> Result<(LuaPolicy, BTreeSet<String>), ExtensionError> {
    let manifest = source.files.get("manifest.json").ok_or_else(|| {
        ExtensionError::new(format!(
            "extension {} is missing manifest.json",
            source.extension_id
        ))
    })?;
    let manifest = parse_manifest(manifest, source)?;
    let bundle = Bundle::from_sources(
        BundleManifest::new(
            manifest.abi_version,
            &manifest.entrypoint,
            manifest.requested_capabilities.iter().map(String::as_str),
        )
        .map_err(extension_error)?,
        manifest
            .modules
            .iter()
            .map(|module| {
                source
                    .files
                    .get(module)
                    .cloned()
                    .map(|contents| (module.clone(), contents))
                    .ok_or_else(|| {
                        ExtensionError::new(format!(
                            "extension {} is missing declared module {module}",
                            source.extension_id
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?,
    )
    .map_err(extension_error)?;
    let requested_capabilities = manifest.requested_capabilities.clone();
    LuaPolicy::load_bundle_with_limits(
        bundle,
        PolicyLimits {
            max_source_bytes: manifest.limits.max_source_bytes,
            max_memory_bytes: manifest.limits.max_memory_bytes,
            max_interrupt_checks: manifest.limits.max_interrupt_checks,
        },
    )
    .map(|policy| (policy, requested_capabilities))
    .map_err(extension_error)
}

struct ParsedManifest {
    abi_version: u32,
    entrypoint: String,
    modules: Vec<String>,
    requested_capabilities: BTreeSet<String>,
    limits: ExtensionLimits,
}

fn parse_manifest(
    manifest: &str,
    source: &ExtensionSourceTree,
) -> Result<ParsedManifest, ExtensionError> {
    let value = JsonValue::parse(manifest).map_err(|error| {
        ExtensionError::new(format!(
            "extension {} manifest is invalid JSON: {error}",
            source.extension_id
        ))
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| ExtensionError::new("extension manifest must be an object"))?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "schema_version"
                | "abi_version"
                | "id"
                | "entrypoint"
                | "modules"
                | "requested_capabilities"
                | "resource_limits"
        ) {
            return Err(ExtensionError::new(format!(
                "extension manifest has unknown field {key}"
            )));
        }
    }
    if required_u64(object, "schema_version")? != 1 {
        return Err(ExtensionError::new(
            "extension manifest schema_version must be 1",
        ));
    }
    let abi_version = required_u64(object, "abi_version")? as u32;
    if abi_version != BUNDLE_ABI_VERSION {
        return Err(ExtensionError::new(format!(
            "extension manifest selects unsupported ABI {abi_version}"
        )));
    }
    if required_string(object, "id")? != source.extension_id {
        return Err(ExtensionError::new(
            "extension manifest identity disagrees with immutable registry",
        ));
    }
    let entrypoint = required_string(object, "entrypoint")?.to_owned();
    let modules = required_strings(object, "modules")?;
    let requested_capabilities = required_strings(object, "requested_capabilities")?
        .into_iter()
        .collect::<BTreeSet<_>>();
    if source
        .expected_capabilities
        .as_ref()
        .is_some_and(|expected| *expected != requested_capabilities)
    {
        return Err(ExtensionError::new(
            "extension manifest capabilities disagree with immutable registry",
        ));
    }
    let limits = match object.get("resource_limits") {
        None => source.limits,
        Some(value) => parse_resource_limits(value, source)?,
    };
    let declared = modules.iter().cloned().collect::<BTreeSet<_>>();
    if declared.len() != modules.len() {
        return Err(ExtensionError::new(
            "extension manifest repeats a declared module",
        ));
    }
    if let Some(path) = source
        .files
        .keys()
        .find(|path| path.as_str() != "manifest.json" && !declared.contains(*path))
    {
        return Err(ExtensionError::new(format!(
            "extension source tree contains undeclared module {path}"
        )));
    }
    Ok(ParsedManifest {
        abi_version,
        entrypoint,
        modules,
        requested_capabilities,
        limits,
    })
}

fn parse_resource_limits(
    value: &JsonValue,
    source: &ExtensionSourceTree,
) -> Result<ExtensionLimits, ExtensionError> {
    let object = value
        .as_object()
        .ok_or_else(|| ExtensionError::new("extension resource_limits must be an object"))?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "source_bytes" | "memory_bytes" | "instruction_checks"
        ) {
            return Err(ExtensionError::new(format!(
                "extension resource_limits has unknown field {key}"
            )));
        }
    }
    let limits = ExtensionLimits {
        max_source_bytes: required_u64(object, "source_bytes")? as usize,
        max_memory_bytes: required_u64(object, "memory_bytes")? as usize,
        max_interrupt_checks: required_u64(object, "instruction_checks")? as usize,
    };
    if limits.max_source_bytes == 0
        || limits.max_memory_bytes == 0
        || limits.max_interrupt_checks == 0
    {
        return Err(ExtensionError::new(
            "extension resource_limits must all be greater than zero",
        ));
    }
    if limits.max_source_bytes > source.limits.max_source_bytes
        || limits.max_memory_bytes > source.limits.max_memory_bytes
        || limits.max_interrupt_checks > source.limits.max_interrupt_checks
    {
        return Err(ExtensionError::new(format!(
            "extension {} resource limits exceed its frozen harness limits",
            source.extension_id
        )));
    }
    Ok(limits)
}

fn required_string<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<&'a str, ExtensionError> {
    match object.get(field) {
        Some(JsonValue::String(value)) if !value.is_empty() => Ok(value),
        _ => Err(ExtensionError::new(format!(
            "extension manifest field {field} must be a non-empty string"
        ))),
    }
}

fn required_u64(object: &BTreeMap<String, JsonValue>, field: &str) -> Result<u64, ExtensionError> {
    match object.get(field) {
        Some(JsonValue::Number(JsonNumber::Unsigned(value))) => Ok(*value),
        Some(JsonValue::Number(JsonNumber::Signed(value))) if *value >= 0 => Ok(*value as u64),
        _ => Err(ExtensionError::new(format!(
            "extension manifest field {field} must be a non-negative integer"
        ))),
    }
}

fn required_strings(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Vec<String>, ExtensionError> {
    let JsonValue::Array(values) = object
        .get(field)
        .ok_or_else(|| ExtensionError::new(format!("extension manifest is missing {field}")))?
    else {
        return Err(ExtensionError::new(format!(
            "extension manifest field {field} must be an array"
        )));
    };
    values
        .iter()
        .map(|value| match value {
            JsonValue::String(value) if !value.is_empty() => Ok(value.clone()),
            _ => Err(ExtensionError::new(format!(
                "extension manifest field {field} must contain non-empty strings"
            ))),
        })
        .collect()
}

fn extension_error(error: impl std::fmt::Display) -> ExtensionError {
    ExtensionError::new(error.to_string())
}

#[allow(dead_code)]
fn _memory_proposal_boundary(_proposal: CollectedExtensionMemoryProposal) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tea_core::harness::extension::ExtensionLimits;

    #[test]
    fn engine_derives_a_provider_visible_descriptor_from_closed_source() {
        let source = ExtensionSourceTree {
            extension_id: "fixture.engine".into(),
            files: [
                (
                    "manifest.json".into(),
                    r#"{"schema_version":1,"abi_version":1,"id":"fixture.engine","entrypoint":"main.luau","modules":["main.luau"],"requested_capabilities":[]}"#.into(),
                ),
                (
                    "main.luau".into(),
                    r#"return { prompt_sections = {{ id = "fixture", content = "closed" }} }"#.into(),
                ),
            ]
            .into_iter()
            .collect(),
            expected_capabilities: Some(BTreeSet::new()),
            limits: ExtensionLimits {
                max_source_bytes: 4096,
                max_memory_bytes: 1_048_576,
                max_interrupt_checks: 1_000,
            },
        };

        let descriptor = LuauExtensionEngine
            .describe(&source)
            .expect("closed source is valid");
        assert_eq!(
            descriptor.prompt_sections,
            vec![ExtensionPromptSection {
                id: "fixture".into(),
                content: "closed".into(),
            }]
        );
        assert!(descriptor.tools.is_empty());
    }
}
