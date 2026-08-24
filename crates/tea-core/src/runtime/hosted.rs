//! Sessionless execution of one already-resolved immutable harness epoch.
//!
//! A hosted epoch is the embedding seam for an external durable authority. It
//! uses Tea's ordinary resolved-harness and agent-construction contracts while
//! deliberately creating no Tea session, artifact tools, authoring tools,
//! files, tasks, providers, or capability bindings.

use crate::agent::Agent;
use crate::effect::{EffectGate, RunProvenance};
use crate::harness::{HarnessError, HarnessSurfaceFingerprints, ResolvedHarness};
use crate::tool::ToolRegistry;
use std::sync::Arc;

use super::{HarnessIdentity, RuntimeServices};

/// Caller-owned executable inputs for one sessionless hosted epoch.
pub struct HostedEpochInput {
    /// Explicit host effect boundary. Tea never invents outer durability.
    pub effect_gate: Arc<dyn EffectGate>,
    /// External run attribution. Harness-owned fields are populated or
    /// checked against the resolved immutable harness.
    pub provenance: RunProvenance,
    /// Explicit epoch-local tools in addition to trusted and extension tools.
    pub additional_tools: ToolRegistry,
}

impl std::fmt::Debug for HostedEpochInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedEpochInput")
            .field("provenance", &self.provenance)
            .field("additional_tools", &self.additional_tools)
            .finish_non_exhaustive()
    }
}

/// One fully constructed Tea agent under an external durable authority.
pub struct HostedEpoch {
    agent: Agent,
    identity: HarnessIdentity,
    surfaces: HarnessSurfaceFingerprints,
    provenance: RunProvenance,
}

impl std::fmt::Debug for HostedEpoch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedEpoch")
            .field("identity", &self.identity)
            .field("surfaces", &self.surfaces)
            .field("provenance", &self.provenance)
            .finish_non_exhaustive()
    }
}

impl HostedEpoch {
    /// Borrow the sessionless agent for caller-driven execution.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Consume the hosted wrapper and return its caller-driven agent.
    pub fn into_agent(self) -> Agent {
        self.agent
    }

    /// Borrow Tea's exact immutable harness identity for this epoch.
    pub fn identity(&self) -> &HarnessIdentity {
        &self.identity
    }

    /// Borrow Tea's standard provider and policy surface fingerprints.
    pub fn surface_fingerprints(&self) -> &HarnessSurfaceFingerprints {
        &self.surfaces
    }

    /// Borrow the normalized run attribution installed on every effect.
    pub fn provenance(&self) -> &RunProvenance {
        &self.provenance
    }
}

impl RuntimeServices {
    /// Prepare one sessionless agent from an already-resolved harness.
    ///
    /// Context and durable lifecycle policies require Tea session semantics.
    /// The stateless hosted path rejects them rather than silently discarding
    /// their contributions. A future stateful hosted policy port can extend
    /// this contract without weakening this fail-closed default.
    pub fn prepare_hosted_epoch(
        &self,
        harness: &ResolvedHarness,
        mut input: HostedEpochInput,
    ) -> Result<HostedEpoch, HarnessError> {
        if !harness.context_policies.is_empty() {
            return Err(HarnessError::invalid_state(
                "hosted epoch requires an explicit context-policy host",
            ));
        }
        if !harness.lifecycle.is_empty() {
            return Err(HarnessError::invalid_state(
                "hosted epoch requires an explicit lifecycle-policy host",
            ));
        }
        let snapshot = harness.harness_snapshot.as_ref().ok_or_else(|| {
            HarnessError::invalid_state(
                "hosted epoch requires the immutable snapshot that produced the resolved harness",
            )
        })?;
        let identity = harness.identity.clone();
        install_harness_provenance(
            &mut input.provenance,
            &identity,
            snapshot.fingerprints.provider_surface_digest.to_hex(),
        )?;
        let provenance = input.provenance;
        let agent = self.build_agent_with_tools(
            harness,
            input.effect_gate,
            provenance.clone(),
            input.additional_tools,
        )?;
        Ok(HostedEpoch {
            agent,
            identity,
            surfaces: snapshot.fingerprints.clone(),
            provenance,
        })
    }
}

fn install_harness_provenance(
    provenance: &mut RunProvenance,
    identity: &HarnessIdentity,
    provider_surface_digest: String,
) -> Result<(), HarnessError> {
    install_exact(
        &mut provenance.harness_snapshot_id,
        identity.snapshot_id().to_string(),
        "harness snapshot",
    )?;
    install_exact(
        &mut provenance.harness_revision_id,
        identity.revision_id().to_string(),
        "harness revision",
    )?;
    install_exact(
        &mut provenance.model_harness_profile_id,
        identity.profile_id().to_string(),
        "model-harness profile",
    )?;
    install_exact(
        &mut provenance.provider_surface_digest,
        provider_surface_digest,
        "provider surface",
    )
}

fn install_exact(
    field: &mut Option<String>,
    expected: String,
    label: &str,
) -> Result<(), HarnessError> {
    if let Some(actual) = field.as_ref()
        && actual != &expected
    {
        return Err(HarnessError::invalid_state(format!(
            "hosted epoch {label} provenance disagrees with the resolved harness",
        )));
    }
    *field = Some(expected);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::NoopEffectGate;
    use crate::harness::extension::{
        ExtensionContextInput, ExtensionContextPatch, ExtensionContextPolicy, ExtensionError,
        ExtensionLifecycle, NoExtensions,
    };
    use crate::harness::{
        HarnessActor, HarnessResourceLimits, HarnessSeedBuilder, ModelHarnessProfile,
        PluginBundleRef, SelfExtensionMode,
    };
    use crate::scheduler::{
        CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
    };
    use crate::state::{ModelDescriptor, StopReason, ThinkingLevel};
    use crate::tool::{
        AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tea_protocol::JsonValue;
    use tea_session::{Digest, MemoryArtifactStore};

    struct OneTurnProvider {
        stream: Mutex<Option<ModelStream>>,
    }

    struct FixtureContextPolicy;

    impl ExtensionContextPolicy for FixtureContextPolicy {
        fn project_context(
            &self,
            _input: &ExtensionContextInput,
        ) -> Result<ExtensionContextPatch, ExtensionError> {
            Ok(ExtensionContextPatch::default())
        }
    }

    struct FixtureLifecycle;

    impl ExtensionLifecycle for FixtureLifecycle {
        fn hook_ids(&self) -> Result<Vec<String>, ExtensionError> {
            Ok(vec!["fixture".into()])
        }

        fn before_operation(&self) -> Result<BTreeMap<String, JsonValue>, ExtensionError> {
            Ok(BTreeMap::new())
        }

        fn before_epoch(&self) -> Result<BTreeMap<String, JsonValue>, ExtensionError> {
            Ok(BTreeMap::new())
        }

        fn before_resume(
            &self,
            _operation_data: &BTreeMap<String, JsonValue>,
            _epoch_data: &BTreeMap<String, JsonValue>,
        ) -> Result<(), ExtensionError> {
            Ok(())
        }
    }

    impl ModelProvider for OneTurnProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            let stream = self
                .stream
                .lock()
                .expect("provider mutex")
                .take()
                .expect("one provider turn");
            Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
        }
    }

    #[derive(Debug)]
    struct FixtureTool;

    impl AgentTool for FixtureTool {
        fn name(&self) -> &str {
            "fixture"
        }

        fn description(&self) -> &str {
            "one explicit fixture tool"
        }

        fn schema(&self) -> &JsonValue {
            static SCHEMA: std::sync::LazyLock<JsonValue> =
                std::sync::LazyLock::new(|| JsonValue::parse(r#"{"type":"object"}"#).unwrap());
            &SCHEMA
        }

        fn execute<'a>(
            &'a self,
            call: ToolCall,
            _context: ToolContext,
            _updates: ToolUpdateSink,
        ) -> ToolFuture<'a> {
            Box::pin(std::future::ready(Ok(AgentToolResult {
                tool_call_id: call.id,
                content: "fixture".into(),
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: false,
                failure: None,
            })))
        }
    }

    fn fixture() -> (RuntimeServices, ResolvedHarness) {
        let provider: Arc<dyn ModelProvider> = Arc::new(OneTurnProvider {
            stream: Mutex::new(Some(ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("done".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            })),
        });
        let mut tools = ToolRegistry::default();
        tools.insert(Arc::new(FixtureTool));
        let services = RuntimeServices::new(provider, tools)
            .model(ModelDescriptor {
                provider: "fixture-provider".into(),
                model: "fixture-model".into(),
                revision: Some("fixture-revision".into()),
            })
            .thinking_level(ThinkingLevel::High);
        let profile = ModelHarnessProfile::new(
            "fixture-provider",
            "fixture-model",
            Some("fixture-revision".into()),
            "fixture-prompt",
            "fixture-tools",
            "fixture-compaction",
            "fixture-projection",
        )
        .expect("fixture profile");
        let artifacts: Arc<dyn tea_session::ArtifactStore> =
            Arc::new(MemoryArtifactStore::default());
        let seeded = HarnessSeedBuilder::new(
            artifacts,
            Arc::new(NoExtensions),
            Digest::from_bytes("fixture-base-profile"),
            "fixture system prompt",
            profile,
            SelfExtensionMode::Off,
            HarnessResourceLimits::default(),
            services.runtime_policy_identities(),
        )
        .seed(HarnessActor::Host, 1)
        .expect("fixture harness seeds");
        let revision_id = seeded.revision.revision_id.clone();
        let resolver = crate::harness::HarnessResolver::new(
            seeded.repository,
            services.clone(),
            Default::default(),
        );
        let resolved = resolver
            .resolve_revision(&revision_id)
            .expect("fixture harness resolves");
        (services, resolved)
    }

    fn input(provenance: RunProvenance, additional_tools: ToolRegistry) -> HostedEpochInput {
        HostedEpochInput {
            effect_gate: Arc::new(NoopEffectGate),
            provenance,
            additional_tools,
        }
    }

    #[test]
    fn hosted_and_managed_construction_share_exact_agent_configuration() {
        let (services, resolved) = fixture();
        let managed = services
            .build_agent_with_tools(
                &resolved,
                Arc::new(NoopEffectGate),
                RunProvenance::default(),
                ToolRegistry::default(),
            )
            .expect("managed construction succeeds");
        let hosted = services
            .prepare_hosted_epoch(
                &resolved,
                input(RunProvenance::default(), ToolRegistry::default()),
            )
            .expect("hosted construction succeeds");

        let managed_snapshot = managed.snapshot();
        let hosted_snapshot = hosted.agent().snapshot();
        assert_eq!(
            managed_snapshot.system_prompt,
            hosted_snapshot.system_prompt
        );
        assert_eq!(managed_snapshot.model, hosted_snapshot.model);
        assert_eq!(
            managed_snapshot.thinking_level,
            hosted_snapshot.thinking_level
        );
        assert_eq!(
            managed.tool_definitions(),
            hosted.agent().tool_definitions()
        );
        assert_eq!(
            hosted
                .surface_fingerprints()
                .provider_surface_digest
                .to_hex(),
            hosted
                .provenance()
                .provider_surface_digest
                .as_deref()
                .expect("hosted provider surface is attached"),
        );
        assert_eq!(hosted.agent().tool_definitions().len(), 1);
        assert!(hosted.agent().tool_definitions().iter().all(|tool| {
            !matches!(
                tool.name.as_str(),
                "tea_harness" | "tea_artifact_read" | "tea_artifact_search" | "tea_history_search"
            )
        }));
    }

    #[test]
    fn hosted_provenance_disagreement_and_tool_collisions_fail_closed() {
        let (services, resolved) = fixture();
        let error = services
            .prepare_hosted_epoch(
                &resolved,
                input(
                    RunProvenance {
                        harness_snapshot_id: Some("wrong-snapshot".into()),
                        ..RunProvenance::default()
                    },
                    ToolRegistry::default(),
                ),
            )
            .expect_err("forged provenance is rejected");
        assert!(error.to_string().contains("snapshot provenance disagrees"));

        let mut additional = ToolRegistry::default();
        additional.insert(Arc::new(FixtureTool));
        let error = services
            .prepare_hosted_epoch(&resolved, input(RunProvenance::default(), additional))
            .expect_err("additional tool cannot replace a trusted tool");
        assert!(
            error
                .to_string()
                .contains("collides with reserved host tool")
        );

        let (services, mut resolved) = fixture();
        resolved.extension_tools.insert(Arc::new(FixtureTool));
        let error = services
            .prepare_hosted_epoch(
                &resolved,
                input(RunProvenance::default(), ToolRegistry::default()),
            )
            .expect_err("resolved extension tool cannot replace a trusted tool");
        assert!(
            error
                .to_string()
                .contains("collides with a trusted host capability")
        );
    }

    #[test]
    fn hosted_epoch_rejects_runtime_policy_identity_mismatch() {
        let (services, mut resolved) = fixture();
        resolved
            .harness_snapshot
            .as_mut()
            .expect("fixture has an immutable snapshot")
            .spec
            .tool_projection_digest = Digest::from_bytes("hosted-wrong-projection");
        let error = services
            .prepare_hosted_epoch(
                &resolved,
                input(RunProvenance::default(), ToolRegistry::default()),
            )
            .expect_err("hosted construction rejects policy identity drift");
        assert!(error.to_string().contains("tool-result projection identity"));
    }

    #[test]
    fn hosted_epoch_accepts_combined_session_plugin_hook_identity() {
        let (services, mut resolved) = fixture();
        let snapshot = resolved
            .harness_snapshot
            .as_mut()
            .expect("fixture has an immutable snapshot");
        snapshot.spec.ordered_session_plugins.push(PluginBundleRef {
            plugin_id: "fixture-plugin".into(),
            tree_id: tea_session::HarnessTreeId::new("fixture-tree").expect("tree ID"),
            requested_capabilities: std::collections::BTreeSet::new(),
        });
        snapshot.spec.hook_bundle_digest =
            crate::harness::lineage::runtime_hook_bundle_digest(
                services
                    .runtime_policy_identities()
                    .hook_bundle_digest,
                &snapshot.spec,
            );
        services
            .prepare_hosted_epoch(
                &resolved,
                input(RunProvenance::default(), ToolRegistry::default()),
            )
            .expect("combined session-plugin hook identity resolves");
    }

    #[test]
    fn hosted_epoch_rejects_unhandled_context_and_lifecycle_policies() {
        let (services, mut resolved) = fixture();
        resolved.context_policies =
            super::super::context::ContextPolicyRegistry::from_resolved([(
                "fixture".to_owned(),
                Arc::new(FixtureContextPolicy) as Arc<dyn ExtensionContextPolicy>,
            )]);
        let error = services
            .prepare_hosted_epoch(
                &resolved,
                input(RunProvenance::default(), ToolRegistry::default()),
            )
            .expect_err("context policy needs an explicit hosted port");
        assert!(error.to_string().contains("context-policy host"));

        let (services, mut resolved) = fixture();
        resolved.lifecycle = super::super::lifecycle::PluginLifecycleRegistry::from_resolved([(
            "fixture".to_owned(),
            Arc::new(FixtureLifecycle) as Arc<dyn ExtensionLifecycle>,
        )])
        .expect("fixture lifecycle registers");
        let error = services
            .prepare_hosted_epoch(
                &resolved,
                input(RunProvenance::default(), ToolRegistry::default()),
            )
            .expect_err("lifecycle policy needs an explicit hosted port");
        assert!(error.to_string().contains("lifecycle-policy host"));
    }

    #[test]
    fn trace_observer_records_a_caller_driven_hosted_epoch() {
        smol::block_on(async {
            let (services, resolved) = fixture();
            let hosted = services
                .prepare_hosted_epoch(
                    &resolved,
                    input(RunProvenance::default(), ToolRegistry::default()),
                )
                .expect("hosted construction succeeds");
            let trace = Arc::new(crate::trace::TraceObserver::new_with_provenance(
                "hosted-fixture",
                hosted.provenance().clone(),
                Vec::<tea_trace::TraceEvent>::new(),
            ));
            let _subscription = hosted.agent().subscribe(trace.clone());
            hosted
                .agent()
                .start_prompt("fixture assignment")
                .expect("hosted run starts")
                .drive()
                .await
                .expect("hosted run settles");
            trace.with_sink(|events| {
                assert!(matches!(
                    events.first(),
                    Some(tea_trace::TraceEvent::EpisodeHeader(_))
                ));
                assert!(matches!(
                    events.last(),
                    Some(tea_trace::TraceEvent::EpisodeEnd(_))
                ));
            });
        });
    }
}
