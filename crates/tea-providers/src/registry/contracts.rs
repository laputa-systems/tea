//! Provider registry contracts and selection types.

use crate::scheduler::ModelProvider;
use std::fmt;
use std::sync::Arc;
/// Version of the checked-in picker metadata format.
pub const MODEL_CATALOG_VERSION: u32 = 1;

/// One picker-visible model in a provider's checked-in catalog.
///
/// The catalog is deliberately a small, versioned list of identifiers already present in this
/// repository. `context_window` is `None` when this repository does not provide an authoritative
/// context-capacity source; the registry does not infer one from a model name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelDescriptor {
    /// Stable provider-local model identifier.
    pub id: &'static str,
    /// Human-readable name for a host picker.
    pub display_name: &'static str,
    /// Known context capacity in tokens, if supplied by repository source data.
    pub context_window: Option<u64>,
}

/// Provider capabilities that are safe for a host to advertise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderCapabilities {
    /// Whether this adapter can expose provider-reported monetary cost.
    pub provider_reported_cost: bool,
    /// Whether this adapter currently has a concrete provider-backed compactor.
    ///
    /// Built-in adapters remain `false` until a host installs a documented provider-backed
    /// compactor policy for that adapter. This flag is metadata only; it never creates an implicit
    /// fallback merely because the core exposes the generic compactor port.
    pub concrete_compactor: bool,
}

impl ProviderCapabilities {
    /// Whether provider-reported cost is available.
    pub const fn supports_provider_reported_cost(self) -> bool {
        self.provider_reported_cost
    }

    /// Whether a concrete provider-backed compactor is available.
    pub const fn supports_compaction(self) -> bool {
        self.concrete_compactor
    }
}

/// The explicit configuration family required by one compiled adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderConfigurationKind {
    /// Command Code's caller-owned API key and host context configuration.
    #[cfg(feature = "provider-commandcode")]
    CommandCode,
    /// OpenRouter's caller-owned API key and model configuration.
    #[cfg(feature = "provider-openrouter")]
    OpenRouter,
    /// Local OpenAI-compatible endpoint and model configuration.
    #[cfg(feature = "provider-local")]
    Local,
}

/// Metadata for one adapter compiled into this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderEntry {
    /// Stable provider identifier used in model descriptors.
    pub id: &'static str,
    /// Human-readable provider name for a host picker.
    pub display_name: &'static str,
    /// Version of the static model list below.
    pub model_catalog_version: u32,
    /// Picker-visible models. This list is not a promise of a vendor's complete catalog.
    pub models: &'static [ModelDescriptor],
    /// Whether a caller may supply a model identifier outside `models`.
    pub allows_custom_models: bool,
    /// The explicit adapter configuration family accepted by [`ProviderRegistry::build`].
    pub configuration: ProviderConfigurationKind,
    /// Capabilities available from this adapter.
    pub capabilities: ProviderCapabilities,
}

impl ProviderEntry {
    /// Find one static catalog model by exact identifier.
    pub fn model(&self, model_id: &str) -> Option<&'static ModelDescriptor> {
        self.models.iter().find(|model| model.id == model_id)
    }

    /// Whether a model identifier may be supplied through the custom-model path.
    pub const fn allows_custom_model(&self) -> bool {
        self.allows_custom_models
    }
}

/// A model selected from the static catalog or through the explicit custom-model path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelection {
    /// Provider-independent descriptor ready for an agent configuration.
    pub descriptor: crate::state::ModelDescriptor,
    /// Whether this selection was outside the checked-in catalog.
    pub custom: bool,
}

impl ModelSelection {
    /// Borrow the provider-independent descriptor.
    pub const fn descriptor(&self) -> &crate::state::ModelDescriptor {
        &self.descriptor
    }

    /// Consume the selection and return its provider-independent descriptor.
    pub fn into_descriptor(self) -> crate::state::ModelDescriptor {
        self.descriptor
    }
}

/// Explicit caller-owned configuration for one compiled adapter.
///
/// The enum is empty in a default provider-free build. No default constructor or environment
/// lookup can manufacture credentials or host context.
#[derive(Clone, Debug)]
pub enum ProviderConfiguration {
    /// Fully configured Command Code adapter.
    #[cfg(feature = "provider-commandcode")]
    CommandCode(crate::commandcode::CommandCodeConfig),
    /// Fully configured OpenRouter adapter.
    #[cfg(feature = "provider-openrouter")]
    OpenRouter(crate::openrouter::OpenRouterConfig),
    /// Fully configured local OpenAI-compatible adapter.
    #[cfg(feature = "provider-local")]
    Local(crate::local::LocalConfig),
}

/// A provider and the exact model descriptor it was configured to serve.
pub struct ConfiguredProvider {
    /// Descriptor selected by the host and validated against the registry.
    pub descriptor: crate::state::ModelDescriptor,
    /// Explicitly constructed provider adapter.
    pub provider: Arc<dyn ModelProvider>,
}

impl fmt::Debug for ConfiguredProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfiguredProvider")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

/// Errors from model resolution or explicit adapter construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryError {
    /// The requested provider is not compiled into this crate.
    UnknownProvider {
        /// Provider ID supplied by the caller.
        provider: String,
    },
    /// A required model identifier was empty.
    EmptyModel {
        /// Provider ID whose model was empty.
        provider: String,
    },
    /// The model is not in the static catalog and custom IDs are unavailable.
    UnknownModel {
        /// Provider ID selected by the caller.
        provider: String,
        /// Model ID rejected by the catalog.
        model: String,
    },
    /// The explicit configuration belongs to a different provider family.
    ConfigurationProviderMismatch {
        /// Provider selected by the caller.
        expected: String,
        /// Provider family represented by the supplied configuration.
        actual: &'static str,
    },
    /// The explicit configuration's model does not match the selected descriptor.
    ConfigurationModelMismatch {
        /// Model selected by the caller.
        expected: String,
        /// Model represented by the supplied configuration.
        actual: String,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProvider { provider } => {
                write!(formatter, "unknown provider {provider:?}")
            }
            Self::EmptyModel { provider } => {
                write!(
                    formatter,
                    "model for provider {provider:?} must not be empty"
                )
            }
            Self::UnknownModel { provider, model } => {
                write!(formatter, "unknown model {provider}/{model}")
            }
            Self::ConfigurationProviderMismatch { expected, actual } => write!(
                formatter,
                "provider configuration is for {actual}, selected provider is {expected}"
            ),
            Self::ConfigurationModelMismatch { expected, actual } => write!(
                formatter,
                "provider configuration model {actual:?} does not match selected model {expected:?}"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}
