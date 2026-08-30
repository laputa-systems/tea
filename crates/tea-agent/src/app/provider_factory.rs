//! Lazy terminal-owned provider construction and fixed child model policy.
//!
//! The static registry answers which models may be selected; this factory owns
//! the process-local authority required to configure an adapter.  In
//! particular, it does not read credentials until a descriptor is actually
//! selected, and cached adapters never cross an exact descriptor boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use tea_core::runtime::{SubagentModel, SubagentPolicy};
use tea_core::state::ModelDescriptor;
use tea_providers::{ConfiguredProvider, ProviderConfiguration, ProviderRegistry};

use super::compaction::ProviderCompactor;
use super::config::SubagentTuiConfig;
use super::error::AppError;
use super::mock;

/// One exact provider/model/revision identity suitable for deterministic host caches.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProviderDescriptorKey {
    provider: String,
    model: String,
    revision: Option<String>,
}

impl From<&ModelDescriptor> for ProviderDescriptorKey {
    fn from(descriptor: &ModelDescriptor) -> Self {
        Self {
            provider: descriptor.provider.clone(),
            model: descriptor.model.clone(),
            revision: descriptor.revision.clone(),
        }
    }
}

/// Host-owned credential boundary. Adapters never receive ambient authority.
trait CredentialSource: Send + Sync {
    fn load(&self, variable: &'static str, provider_name: &'static str)
        -> Result<String, AppError>;
}

#[derive(Debug, Default)]
struct EnvironmentCredentials;

impl CredentialSource for EnvironmentCredentials {
    fn load(
        &self,
        variable: &'static str,
        provider_name: &'static str,
    ) -> Result<String, AppError> {
        std::env::var(variable)
            .map_err(|_| AppError::Setup(format!("{variable} is required for {provider_name}")))
    }
}

/// Reusable terminal-owned provider factory.
///
/// Construction is side-effect free: the static registry, selected local
/// endpoint, and logical workspace are captured without touching credentials
/// or adapter transports. The cache is keyed by the complete descriptor so a
/// selected child model cannot mutate another lane's adapter configuration.
pub(super) struct ProviderFactory {
    registry: ProviderRegistry,
    local_base_url: Option<String>,
    local_context_window: Option<NonZeroU64>,
    logical_workspace: String,
    credentials: Arc<dyn CredentialSource>,
    cache: Mutex<BTreeMap<ProviderDescriptorKey, Arc<ConfiguredProvider>>>,
    compactors: Mutex<BTreeMap<ProviderDescriptorKey, Arc<ProviderCompactor>>>,
}

impl std::fmt::Debug for ProviderFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderFactory")
            .field("registry", &self.registry)
            .field("local_base_url", &self.local_base_url)
            .field("local_context_window", &self.local_context_window)
            .field("logical_workspace", &self.logical_workspace)
            .field(
                "cached_adapter_count",
                &self.cache.lock().map(|cache| cache.len()),
            )
            .finish_non_exhaustive()
    }
}

impl ProviderFactory {
    /// Create a lazy factory from explicit terminal-owned inputs.
    pub(super) fn new(
        registry: ProviderRegistry,
        local_base_url: Option<String>,
        local_context_window: Option<NonZeroU64>,
        logical_workspace: String,
    ) -> Self {
        Self::with_credentials(
            registry,
            local_base_url,
            local_context_window,
            logical_workspace,
            Arc::new(EnvironmentCredentials),
        )
    }

    fn with_credentials(
        registry: ProviderRegistry,
        local_base_url: Option<String>,
        local_context_window: Option<NonZeroU64>,
        logical_workspace: String,
        credentials: Arc<dyn CredentialSource>,
    ) -> Self {
        Self {
            registry,
            local_base_url,
            local_context_window,
            logical_workspace,
            credentials,
            cache: Mutex::new(BTreeMap::new()),
            compactors: Mutex::new(BTreeMap::new()),
        }
    }

    /// Resolve the fixed, ordered child model catalog authorized for one root model.
    pub(super) fn resolve_subagent_policy(
        &self,
        root: &ModelDescriptor,
        config: &SubagentTuiConfig,
    ) -> Result<SubagentPolicy, AppError> {
        let provider_id = config.provider.as_deref().unwrap_or(&root.provider);
        if provider_id == mock::PROVIDER_ID {
            return self.resolve_mock_subagent_policy(config);
        }
        let provider = self.registry.provider(provider_id).ok_or_else(|| {
            AppError::Setup(format!(
                "subagent provider {provider_id:?} is not compiled in"
            ))
        })?;
        let models = match config.models.as_ref() {
            Some(identifiers) => {
                let mut seen = BTreeSet::new();
                let mut resolved = Vec::with_capacity(identifiers.len());
                for model_id in identifiers {
                    if model_id.trim().is_empty() {
                        return Err(AppError::Setup(format!(
                            "subagent model for provider {provider_id:?} must not be empty"
                        )));
                    }
                    if !seen.insert(model_id) {
                        return Err(AppError::Setup(format!(
                            "subagent model catalog contains duplicate model {model_id:?}"
                        )));
                    }
                    let model = provider.model(model_id).ok_or_else(|| {
                        AppError::Setup(format!(
                            "subagent model {model_id:?} is not a checked-in model for provider {provider_id:?}"
                        ))
                    })?;
                    resolved.push(self.catalog_model(provider_id, model));
                }
                resolved
            }
            None => {
                let mut resolved = provider
                    .models
                    .iter()
                    .map(|model| self.catalog_model(provider_id, model))
                    .collect::<Vec<_>>();
                if root.provider == provider_id
                    && provider.model(&root.model).is_none()
                    && self
                        .registry
                        .custom_model(provider_id, root.model.clone())?
                        .custom
                {
                    resolved.push(SubagentModel {
                        descriptor: root.clone(),
                        display_name: root.model.clone(),
                        context_window: self.context_window(root),
                    });
                }
                resolved
            }
        };
        let policy = SubagentPolicy {
            models,
            max_concurrent: config.max_concurrent,
            max_total_per_operation: config.max_total_per_operation,
            timeout: config.timeout,
        };
        policy
            .validate()
            .map_err(|error| AppError::Setup(format!("invalid subagent policy: {error}")))?;
        Ok(policy)
    }

    /// The terminal-only mock adapter is intentionally outside the checked-in
    /// production registry. It still has a closed, one-model child catalog so
    /// enabled one-shot and PTY paths can be exercised without credentials or
    /// network authority.
    fn resolve_mock_subagent_policy(
        &self,
        config: &SubagentTuiConfig,
    ) -> Result<SubagentPolicy, AppError> {
        if let Some(models) = &config.models {
            if models.len() != 1 || models[0] != mock::DEFAULT_MODEL_ID {
                return Err(AppError::Setup(format!(
                    "subagent model is not in the fixed mock catalog: {:?}",
                    models.first().map(String::as_str).unwrap_or("<empty>")
                )));
            }
        }
        let descriptor = ModelDescriptor {
            provider: mock::PROVIDER_ID.into(),
            model: mock::DEFAULT_MODEL_ID.into(),
            revision: None,
        };
        let policy = SubagentPolicy {
            models: vec![SubagentModel {
                descriptor,
                display_name: "Safe TUI playground".into(),
                context_window: NonZeroU64::new(mock::CONTEXT_WINDOW),
            }],
            max_concurrent: config.max_concurrent,
            max_total_per_operation: config.max_total_per_operation,
            timeout: config.timeout,
        };
        policy
            .validate()
            .map_err(|error| AppError::Setup(format!("invalid subagent policy: {error}")))?;
        Ok(policy)
    }

    /// Lazily configure and cache the exact adapter for one selected descriptor.
    pub(super) fn configured(
        &self,
        descriptor: &ModelDescriptor,
    ) -> Result<Arc<ConfiguredProvider>, AppError> {
        let key = ProviderDescriptorKey::from(descriptor);
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| AppError::Setup("provider adapter cache lock is poisoned".into()))?;
        if let Some(configured) = cache.get(&key) {
            return Ok(Arc::clone(configured));
        }
        let configured = Arc::new(self.build(descriptor)?);
        cache.insert(key, Arc::clone(&configured));
        Ok(configured)
    }

    /// Return the one immutable compactor assigned to an exact adapter descriptor.
    pub(super) fn compactor(
        &self,
        configured: &ConfiguredProvider,
    ) -> Result<Arc<ProviderCompactor>, AppError> {
        let key = ProviderDescriptorKey::from(&configured.descriptor);
        let mut compactors = self
            .compactors
            .lock()
            .map_err(|_| AppError::Setup("provider compactor cache lock is poisoned".into()))?;
        if let Some(compactor) = compactors.get(&key) {
            return Ok(Arc::clone(compactor));
        }
        let compactor = Arc::new(ProviderCompactor::new(
            configured.descriptor.clone(),
            Arc::clone(&configured.provider),
        ));
        compactors.insert(key, Arc::clone(&compactor));
        Ok(compactor)
    }

    /// Return the host-known context capacity for a selected descriptor.
    pub(super) fn context_window(&self, descriptor: &ModelDescriptor) -> Option<NonZeroU64> {
        if descriptor.provider == mock::PROVIDER_ID {
            return NonZeroU64::new(mock::CONTEXT_WINDOW);
        }
        if descriptor.provider == "local" {
            return self.local_context_window.or_else(|| {
                self.registry
                    .provider("local")
                    .and_then(|provider| provider.model(&descriptor.model))
                    .and_then(|model| model.context_window)
                    .and_then(NonZeroU64::new)
            });
        }
        self.registry
            .provider(&descriptor.provider)
            .and_then(|provider| provider.model(&descriptor.model))
            .and_then(|model| model.context_window)
            .and_then(NonZeroU64::new)
    }

    fn catalog_model(
        &self,
        provider_id: &str,
        model: &tea_providers::ModelDescriptor,
    ) -> SubagentModel {
        let descriptor = ModelDescriptor {
            provider: provider_id.into(),
            model: model.id.into(),
            revision: None,
        };
        SubagentModel {
            context_window: self.context_window(&descriptor),
            descriptor,
            display_name: model.display_name.into(),
        }
    }

    fn build(&self, descriptor: &ModelDescriptor) -> Result<ConfiguredProvider, AppError> {
        if descriptor.provider == mock::PROVIDER_ID {
            let mut configured = mock::configured_provider(&descriptor.model);
            configured.descriptor = descriptor.clone();
            return Ok(configured);
        }
        self.registry
            .resolve_model(&descriptor.provider, descriptor.model.clone())?;
        let configuration = match descriptor.provider.as_str() {
            "openrouter" => {
                let key = self.credentials.load("OPENROUTER_API_KEY", "OpenRouter")?;
                ProviderConfiguration::OpenRouter(
                    tea_providers::openrouter::OpenRouterConfig::try_new(key, &descriptor.model)
                        .map_err(|error| AppError::Setup(error.to_string()))?,
                )
            }
            "opencode-zen" => {
                let key = self.credentials.load("OPENCODE_API_KEY", "OpenCode Zen")?;
                ProviderConfiguration::OpencodeZen(
                    tea_providers::opencode_zen::OpencodeZenConfig::try_new(key, &descriptor.model)
                        .map_err(|error| AppError::Setup(error.to_string()))?,
                )
            }
            "local" => {
                let base_url = self
                    .local_base_url
                    .clone()
                    .unwrap_or_else(|| tea_providers::local::DEFAULT_BASE_URL.to_owned());
                ProviderConfiguration::Local(
                    tea_providers::local::LocalConfig::try_new(base_url, &descriptor.model)
                        .map_err(|error| AppError::Setup(error.to_string()))?,
                )
            }
            _ => {
                return Err(AppError::Setup(format!(
                    "provider {:?} is not compiled in",
                    descriptor.provider
                )));
            }
        };
        self.registry
            .build(descriptor.clone(), configuration)
            .map_err(Into::into)
    }

    #[cfg(test)]
    fn cached_adapter_count(&self) -> usize {
        self.cache
            .lock()
            .expect("provider adapter cache lock")
            .len()
    }

    #[cfg(test)]
    fn local_configuration_for(
        &self,
        descriptor: &ModelDescriptor,
    ) -> Result<ProviderConfiguration, AppError> {
        if descriptor.provider != "local" {
            return Err(AppError::Setup(
                "test helper requires a local descriptor".into(),
            ));
        }
        let base_url = self
            .local_base_url
            .clone()
            .unwrap_or_else(|| tea_providers::local::DEFAULT_BASE_URL.to_owned());
        tea_providers::local::LocalConfig::try_new(base_url, &descriptor.model)
            .map(ProviderConfiguration::Local)
            .map_err(|error| AppError::Setup(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct RecordingCredentials {
        loads: AtomicUsize,
    }

    impl CredentialSource for RecordingCredentials {
        fn load(
            &self,
            _variable: &'static str,
            _provider_name: &'static str,
        ) -> Result<String, AppError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok("test-key".into())
        }
    }

    fn factory(credentials: Arc<dyn CredentialSource>) -> ProviderFactory {
        ProviderFactory::with_credentials(
            ProviderRegistry::new(),
            Some("http://127.0.0.1:12345/v1".into()),
            NonZeroU64::new(48_000),
            "/logical/workspace".into(),
            credentials,
        )
    }

    fn root(provider: &str, model: &str) -> ModelDescriptor {
        ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            revision: None,
        }
    }

    #[test]
    fn subagent_policy_honors_provider_override_and_declared_model_order() {
        let credentials = Arc::new(RecordingCredentials {
            loads: AtomicUsize::new(0),
        });
        let factory = factory(Arc::clone(&credentials) as Arc<dyn CredentialSource>);
        let config = SubagentTuiConfig {
            provider: Some("openrouter".into()),
            models: Some(vec![
                "openai/gpt-5.6-luna".into(),
                "inclusionai/ling-3.0-tiny:free".into(),
            ]),
            ..SubagentTuiConfig::default()
        };

        let policy = factory
            .resolve_subagent_policy(
                &root("local", tea_providers::local::LAGUNA_XS_2_1_MODEL),
                &config,
            )
            .expect("configured provider catalog resolves");
        assert_eq!(
            policy
                .models
                .iter()
                .map(|model| (
                    model.descriptor.provider.as_str(),
                    model.descriptor.model.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("openrouter", "openai/gpt-5.6-luna"),
                ("openrouter", "inclusionai/ling-3.0-tiny:free"),
            ]
        );
        assert_eq!(
            credentials.loads.load(Ordering::SeqCst),
            0,
            "policy resolution must not load credentials for unselected child models"
        );
    }

    #[test]
    fn subagent_policy_inherits_registry_order_and_appends_a_custom_root_model_once() {
        let factory = factory(Arc::new(RecordingCredentials {
            loads: AtomicUsize::new(0),
        }));
        let root = root("openrouter", "caller/private-model");
        let policy = factory
            .resolve_subagent_policy(&root, &SubagentTuiConfig::default())
            .expect("root provider catalog resolves");
        let registry = ProviderRegistry::new();
        let entry = registry
            .provider("openrouter")
            .expect("OpenRouter is compiled for tea-agent");
        assert_eq!(policy.models.len(), entry.models.len() + 1);
        assert_eq!(
            policy.models[..entry.models.len()]
                .iter()
                .map(|model| model.descriptor.model.as_str())
                .collect::<Vec<_>>(),
            entry
                .models
                .iter()
                .map(|model| model.id)
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            policy.models.last().expect("custom root model").descriptor,
            root
        );
    }

    #[test]
    fn mock_uses_its_closed_terminal_catalog_without_credentials() {
        let credentials = Arc::new(RecordingCredentials {
            loads: AtomicUsize::new(0),
        });
        let factory = factory(Arc::clone(&credentials) as Arc<dyn CredentialSource>);

        let policy = factory
            .resolve_subagent_policy(
                &root(mock::PROVIDER_ID, mock::DEFAULT_MODEL_ID),
                &SubagentTuiConfig::default(),
            )
            .expect("the terminal mock has one closed child model");
        assert_eq!(policy.models.len(), 1);
        assert_eq!(policy.models[0].descriptor.provider, mock::PROVIDER_ID);
        assert_eq!(policy.models[0].descriptor.model, mock::DEFAULT_MODEL_ID);
        assert_eq!(
            credentials.loads.load(Ordering::SeqCst),
            0,
            "mock policy resolution must not consult credentials"
        );

        let error = factory
            .resolve_subagent_policy(
                &root(mock::PROVIDER_ID, mock::DEFAULT_MODEL_ID),
                &SubagentTuiConfig {
                    models: Some(vec!["unapproved-mock-model".into()]),
                    ..SubagentTuiConfig::default()
                },
            )
            .expect_err("mock children expose only the fixed model");
        assert!(error.to_string().contains("not in the fixed mock catalog"));
    }

    #[test]
    fn subagent_policy_rejects_cross_provider_and_duplicate_allowlist_entries() {
        let factory = factory(Arc::new(RecordingCredentials {
            loads: AtomicUsize::new(0),
        }));
        let cross_provider = SubagentTuiConfig {
            provider: Some("local".into()),
            models: Some(vec!["openai/gpt-5.6-luna".into()]),
            ..SubagentTuiConfig::default()
        };
        assert!(factory
            .resolve_subagent_policy(&root("openrouter", "openai/gpt-5.6-luna"), &cross_provider)
            .expect_err("cross-provider model is rejected")
            .to_string()
            .contains("checked-in model"));

        let duplicates = SubagentTuiConfig {
            provider: Some("openrouter".into()),
            models: Some(vec![
                "openai/gpt-5.6-luna".into(),
                "openai/gpt-5.6-luna".into(),
            ]),
            ..SubagentTuiConfig::default()
        };
        assert!(factory
            .resolve_subagent_policy(&root("openrouter", "openai/gpt-5.6-luna"), &duplicates)
            .expect_err("duplicate explicit model is rejected")
            .to_string()
            .contains("duplicate"));
    }

    #[test]
    fn adapters_are_lazy_and_cached_by_exact_descriptor() {
        let credentials = Arc::new(RecordingCredentials {
            loads: AtomicUsize::new(0),
        });
        let factory = factory(Arc::clone(&credentials) as Arc<dyn CredentialSource>);
        assert_eq!(factory.cached_adapter_count(), 0);

        let local = root("local", tea_providers::local::LAGUNA_XS_2_1_MODEL);
        let first_local = factory
            .configured(&local)
            .expect("local adapter builds without credentials");
        let second_local = factory.configured(&local).expect("local adapter is cached");
        assert!(Arc::ptr_eq(&first_local, &second_local));
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 0);

        let openrouter = root("openrouter", "openai/gpt-5.6-luna");
        let first_openrouter = factory
            .configured(&openrouter)
            .expect("configured adapter builds");
        let second_openrouter = factory
            .configured(&openrouter)
            .expect("configured adapter is cached");
        assert!(Arc::ptr_eq(&first_openrouter, &second_openrouter));
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 1);
        assert_eq!(factory.cached_adapter_count(), 2);
    }

    #[test]
    fn local_endpoint_and_context_override_are_preserved() {
        let factory = factory(Arc::new(RecordingCredentials {
            loads: AtomicUsize::new(0),
        }));
        let descriptor = root("local", "caller/local-model");
        assert_eq!(
            factory.context_window(&descriptor).map(NonZeroU64::get),
            Some(48_000)
        );
        let ProviderConfiguration::Local(configuration) = factory
            .local_configuration_for(&descriptor)
            .expect("local configuration builds")
        else {
            panic!("local descriptor must produce local configuration");
        };
        assert_eq!(configuration.base_url(), "http://127.0.0.1:12345/v1");
        assert_eq!(configuration.model(), "caller/local-model");
    }

    #[test]
    fn compactors_are_immutable_and_cached_per_descriptor() {
        let factory = factory(Arc::new(RecordingCredentials {
            loads: AtomicUsize::new(0),
        }));
        let first = factory
            .configured(&root("local", tea_providers::local::LAGUNA_XS_2_1_MODEL))
            .expect("local adapter builds");
        let second = factory
            .configured(&root("local", "caller/local-model"))
            .expect("custom local adapter builds");
        let first_compactor = factory.compactor(&first).expect("first compactor builds");
        let first_compactor_again = factory
            .compactor(&first)
            .expect("first compactor is cached");
        let second_compactor = factory.compactor(&second).expect("second compactor builds");
        assert!(
            !Arc::ptr_eq(&first, &second),
            "same-provider models require distinct adapter instances"
        );
        assert!(Arc::ptr_eq(&first_compactor, &first_compactor_again));
        assert!(!Arc::ptr_eq(&first_compactor, &second_compactor));
    }

    #[test]
    fn descriptor_revisions_never_share_adapters_or_compactors() {
        let factory = factory(Arc::new(RecordingCredentials {
            loads: AtomicUsize::new(0),
        }));
        let base = root("local", tea_providers::local::LAGUNA_XS_2_1_MODEL);
        let revised = ModelDescriptor {
            revision: Some("revision-2026-08-23".into()),
            ..base.clone()
        };
        let base_provider = factory.configured(&base).expect("base adapter builds");
        let revised_provider = factory
            .configured(&revised)
            .expect("revision-pinned adapter builds");
        assert!(!Arc::ptr_eq(&base_provider, &revised_provider));
        let base_compactor = factory
            .compactor(&base_provider)
            .expect("base compactor builds");
        let revised_compactor = factory
            .compactor(&revised_provider)
            .expect("revision-pinned compactor builds");
        assert!(!Arc::ptr_eq(&base_compactor, &revised_compactor));
    }
}
