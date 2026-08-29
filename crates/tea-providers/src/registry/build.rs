//! Registry resolution and provider construction logic.

use super::catalog::COMPILED_PROVIDERS;
use super::contracts::{
    ConfiguredProvider, ModelSelection, ProviderConfiguration, ProviderEntry, RegistryError,
};
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen"
))]
use std::sync::Arc;

/// Explicit registry of adapters selected by Cargo features.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProviderRegistry {
    entries: &'static [ProviderEntry],
}

impl ProviderRegistry {
    /// Construct a registry from this build's static feature-selected entries.
    pub const fn new() -> Self {
        Self {
            entries: COMPILED_PROVIDERS,
        }
    }

    /// Return all adapters compiled into this build, in stable provider-ID order.
    pub const fn providers(&self) -> &'static [ProviderEntry] {
        self.entries
    }

    /// Find one compiled provider by stable ID.
    pub fn provider(&self, provider_id: &str) -> Option<&ProviderEntry> {
        self.entries.iter().find(|entry| entry.id == provider_id)
    }

    /// Resolve a static model or an allowed custom model ID.
    pub fn resolve_model(
        &self,
        provider_id: &str,
        model_id: impl Into<String>,
    ) -> Result<ModelSelection, RegistryError> {
        let model_id = model_id.into();
        let entry = self
            .provider(provider_id)
            .ok_or_else(|| RegistryError::UnknownProvider {
                provider: provider_id.to_owned(),
            })?;
        if model_id.trim().is_empty() {
            return Err(RegistryError::EmptyModel {
                provider: provider_id.to_owned(),
            });
        }
        let custom = entry.model(&model_id).is_none();
        if custom && !entry.allows_custom_models {
            return Err(RegistryError::UnknownModel {
                provider: provider_id.to_owned(),
                model: model_id,
            });
        }
        Ok(ModelSelection {
            descriptor: crate::state::ModelDescriptor {
                provider: provider_id.to_owned(),
                model: model_id,
                revision: None,
            },
            custom,
        })
    }

    /// Resolve a caller-supplied model that is intentionally outside the static catalog.
    pub fn custom_model(
        &self,
        provider_id: &str,
        model_id: impl Into<String>,
    ) -> Result<ModelSelection, RegistryError> {
        let selection = self.resolve_model(provider_id, model_id)?;
        if !selection.custom {
            return Err(RegistryError::UnknownModel {
                provider: provider_id.to_owned(),
                model: selection.descriptor.model,
            });
        }
        Ok(selection)
    }

    /// Build an adapter from explicit owned configuration and a resolved model descriptor.
    pub fn build(
        &self,
        descriptor: crate::state::ModelDescriptor,
        configuration: ProviderConfiguration,
    ) -> Result<ConfiguredProvider, RegistryError> {
        self.resolve_model(&descriptor.provider, descriptor.model.clone())?;
        match configuration {
            #[cfg(feature = "provider-openrouter")]
            ProviderConfiguration::OpenRouter(configuration) => {
                if descriptor.provider != "openrouter" {
                    return Err(RegistryError::ConfigurationProviderMismatch {
                        expected: descriptor.provider.clone(),
                        actual: "openrouter",
                    });
                }
                if configuration.model() != descriptor.model {
                    return Err(RegistryError::ConfigurationModelMismatch {
                        expected: descriptor.model,
                        actual: configuration.model().to_owned(),
                    });
                }
                Ok(ConfiguredProvider {
                    descriptor,
                    provider: Arc::new(crate::openrouter::OpenRouterProvider::new(configuration)),
                })
            }
            #[cfg(feature = "provider-local")]
            ProviderConfiguration::Local(configuration) => {
                if descriptor.provider != "local" {
                    return Err(RegistryError::ConfigurationProviderMismatch {
                        expected: descriptor.provider.clone(),
                        actual: "local",
                    });
                }
                if configuration.model() != descriptor.model {
                    return Err(RegistryError::ConfigurationModelMismatch {
                        expected: descriptor.model,
                        actual: configuration.model().to_owned(),
                    });
                }
                Ok(ConfiguredProvider {
                    descriptor,
                    provider: Arc::new(crate::local::LocalProvider::new(configuration)),
                })
            }
            #[cfg(feature = "provider-opencode-zen")]
            ProviderConfiguration::OpencodeZen(configuration) => {
                if descriptor.provider != "opencode-zen" {
                    return Err(RegistryError::ConfigurationProviderMismatch {
                        expected: descriptor.provider.clone(),
                        actual: "opencode-zen",
                    });
                }
                if configuration.model() != descriptor.model {
                    return Err(RegistryError::ConfigurationModelMismatch {
                        expected: descriptor.model,
                        actual: configuration.model().to_owned(),
                    });
                }
                Ok(ConfiguredProvider {
                    descriptor,
                    provider: Arc::new(crate::opencode_zen::OpencodeZenProvider::new(configuration)),
                })
            }
        }
    }

    /// Build an adapter from a [`ModelSelection`] returned by this registry.
    pub fn build_selection(
        &self,
        selection: ModelSelection,
        configuration: ProviderConfiguration,
    ) -> Result<ConfiguredProvider, RegistryError> {
        self.build(selection.descriptor, configuration)
    }
}
