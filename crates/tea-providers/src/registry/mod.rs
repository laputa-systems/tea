//! Explicit, feature-gated provider and model metadata.
//!
//! [`ProviderRegistry`] is intentionally only a view over checked-in Rust data. Constructing it
//! does not read the process environment, a credential file, a workspace, or a remote catalog.
//! Hosts resolve credentials and other authority themselves, then pass an owned
//! [`ProviderConfiguration`] to [`ProviderRegistry::build`].

mod build;
mod catalog;
mod contracts;

pub use build::ProviderRegistry;
pub use contracts::{
    ConfiguredProvider, MODEL_CATALOG_VERSION, ModelDescriptor, ModelSelection,
    ProviderCapabilities, ProviderConfiguration, ProviderConfigurationKind, ProviderEntry,
    RegistryError,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_static_and_has_no_ambient_configuration_path() {
        let first = ProviderRegistry::new();
        let second = ProviderRegistry::new();
        assert_eq!(first.providers(), second.providers());
        assert!(
            first
                .providers()
                .iter()
                .all(|entry| entry.model_catalog_version == MODEL_CATALOG_VERSION)
        );
    }

    #[cfg(not(any(feature = "provider-openrouter", feature = "provider-local")))]
    #[test]
    fn default_build_remains_provider_free() {
        assert!(ProviderRegistry::new().providers().is_empty());
    }

    #[cfg(feature = "provider-openrouter")]
    #[test]
    fn openrouter_feature_exposes_reported_cost_and_known_ids() {
        let registry = ProviderRegistry::new();
        let provider = registry.provider("openrouter").expect("compiled provider");
        assert_eq!(provider.display_name, "OpenRouter");
        assert_eq!(
            provider.configuration,
            ProviderConfigurationKind::OpenRouter
        );
        assert!(provider.capabilities.supports_provider_reported_cost());
        assert!(!provider.capabilities.supports_compaction());
        assert_eq!(
            provider
                .model("deepseek/deepseek-v4-flash-0731")
                .expect("checked-in model")
                .context_window,
            Some(1_048_576)
        );
        assert_eq!(
            provider
                .model("inclusionai/ling-3.0-tiny:free")
                .expect("checked-in model")
                .context_window,
            Some(262_144)
        );
        assert_eq!(
            provider
                .model("openai/gpt-5.6-luna")
                .expect("checked-in model")
                .context_window,
            Some(1_050_000)
        );
        assert_eq!(
            provider
                .model("poolside/laguna-s-2.1:free")
                .expect("checked-in model")
                .context_window,
            Some(262_144)
        );
        assert_eq!(
            provider
                .model("poolside/laguna-xs-2.1:free")
                .expect("checked-in model")
                .context_window,
            Some(262_144)
        );
    }

    #[cfg(feature = "provider-openrouter")]
    #[test]
    fn custom_model_path_is_explicit_and_does_not_change_catalog() {
        let registry = ProviderRegistry::new();
        let before = registry.providers()[0].models;
        let selection = registry
            .custom_model("openrouter", "caller/private-model")
            .expect("custom IDs are allowed");
        assert!(selection.custom);
        assert_eq!(selection.descriptor.provider, "openrouter");
        assert_eq!(selection.descriptor.model, "caller/private-model");
        assert_eq!(registry.providers()[0].models, before);
    }

    #[cfg(feature = "provider-openrouter")]
    #[test]
    fn explicit_openrouter_configuration_builds_without_transport() {
        let registry = ProviderRegistry::new();
        let selection = registry
            .resolve_model("openrouter", "openai/gpt-5.6-luna")
            .expect("checked-in model");
        let configured = registry
            .build(
                selection.into_descriptor(),
                ProviderConfiguration::OpenRouter(
                    crate::openrouter::OpenRouterConfig::try_new("test-key", "openai/gpt-5.6-luna")
                        .expect("valid explicit config"),
                ),
            )
            .expect("matching explicit config");
        assert_eq!(configured.descriptor.provider, "openrouter");
        assert_eq!(configured.descriptor.model, "openai/gpt-5.6-luna");
    }

    #[cfg(feature = "provider-local")]
    #[test]
    fn local_feature_exposes_laguna_and_builds_without_transport() {
        let registry = ProviderRegistry::new();
        let provider = registry.provider("local").expect("compiled provider");
        assert_eq!(provider.display_name, "Local OpenAI-compatible server");
        assert_eq!(provider.configuration, ProviderConfigurationKind::Local);
        assert!(!provider.capabilities.supports_provider_reported_cost());
        assert_eq!(provider.models[0].context_window, Some(32_768));

        let selection = registry
            .resolve_model("local", crate::local::LAGUNA_XS_2_1_MODEL)
            .expect("Laguna should be in the local catalog");
        let configured = registry
            .build(
                selection.into_descriptor(),
                ProviderConfiguration::Local(crate::local::LocalConfig::laguna_xs_2_1(
                    crate::local::DEFAULT_BASE_URL,
                )),
            )
            .expect("matching local config");
        assert_eq!(configured.descriptor.provider, "local");
    }
}
