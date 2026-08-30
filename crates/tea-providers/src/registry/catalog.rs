//! Checked-in provider and model catalog entries.

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
use super::MODEL_CATALOG_VERSION;
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen"
))]
use super::contracts::ModelDescriptor;
use super::contracts::ProviderEntry;
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
use super::contracts::{ProviderCapabilities, ProviderConfigurationKind};
// Source/update evidence for these lists is intentionally local and reviewable. Model identifiers
// and context capacities are synchronized from the pinned Pi model registry in
// `~/d/pi/packages/ai/dist/models.generated.js`. OpenRouter values use Pi's
// provider-specific `contextWindow`, rather than a guessed value from the model name.
#[cfg(feature = "provider-openrouter")]
static OPENROUTER_MODELS: &[ModelDescriptor] = &[
    ModelDescriptor {
        id: "deepseek/deepseek-v4-flash-0731",
        display_name: "DeepSeek V4 Flash 0731",
        context_window: Some(1_048_576),
    },
    ModelDescriptor {
        id: "inclusionai/ling-3.0-tiny:free",
        display_name: "InclusionAI Ling 3.0 Tiny (Free)",
        context_window: Some(262_144),
    },
    ModelDescriptor {
        id: "openai/gpt-5.6-luna",
        display_name: "OpenAI GPT 5.6 Luna",
        context_window: Some(1_050_000),
    },
    ModelDescriptor {
        id: "poolside/laguna-s-2.1:free",
        display_name: "Poolside Laguna S 2.1 (Free)",
        context_window: Some(262_144),
    },
    ModelDescriptor {
        id: "poolside/laguna-xs-2.1",
        display_name: "Poolside Laguna XS 2.1",
        context_window: Some(262_144),
    },
    ModelDescriptor {
        id: "poolside/laguna-xs-2.1:free",
        display_name: "Poolside Laguna XS 2.1 (Free)",
        context_window: Some(262_144),
    },
];

#[cfg(feature = "provider-local")]
static LOCAL_MODELS: &[ModelDescriptor] = &[ModelDescriptor {
    id: crate::local::LAGUNA_XS_2_1_MODEL,
    display_name: "Laguna XS 2.1 5-bit (oMLX)",
    context_window: Some(32_768),
}];

#[cfg(feature = "provider-opencode-zen")]
static OPENCODE_ZEN_MODELS: &[ModelDescriptor] = &[ModelDescriptor {
    id: "muse-spark-1.2-contributor-free",
    display_name: "Muse Spark 1.2 Contributor (Free)",
    context_window: Some(262_144),
}];

pub(super) static COMPILED_PROVIDERS: &[ProviderEntry] = &[
    #[cfg(feature = "provider-openrouter")]
    ProviderEntry {
        id: "openrouter",
        display_name: "OpenRouter",
        model_catalog_version: MODEL_CATALOG_VERSION,
        models: OPENROUTER_MODELS,
        allows_custom_models: true,
        configuration: ProviderConfigurationKind::OpenRouter,
        capabilities: ProviderCapabilities {
            provider_reported_cost: true,
            concrete_compactor: false,
        },
    },
    #[cfg(feature = "provider-local")]
    ProviderEntry {
        id: "local",
        display_name: "Local OpenAI-compatible server",
        model_catalog_version: MODEL_CATALOG_VERSION,
        models: LOCAL_MODELS,
        allows_custom_models: true,
        configuration: ProviderConfigurationKind::Local,
        capabilities: ProviderCapabilities {
            provider_reported_cost: false,
            concrete_compactor: false,
        },
    },
    #[cfg(feature = "provider-opencode-zen")]
    ProviderEntry {
        id: "opencode-zen",
        display_name: "OpenCode Zen",
        model_catalog_version: MODEL_CATALOG_VERSION,
        models: OPENCODE_ZEN_MODELS,
        allows_custom_models: true,
        configuration: ProviderConfigurationKind::OpencodeZen,
        capabilities: ProviderCapabilities {
            provider_reported_cost: false,
            concrete_compactor: false,
        },
    },
    #[cfg(feature = "provider-codex")]
    ProviderEntry {
        id: crate::codex::PROVIDER_ID,
        display_name: crate::codex::PROVIDER_LABEL,
        model_catalog_version: MODEL_CATALOG_VERSION,
        // Subscription access is account- and rollout-specific. Tea therefore
        // exposes Codex strictly through the explicit custom-model path rather
        // than claiming a checked-in default model is universally available.
        models: &[],
        allows_custom_models: true,
        configuration: ProviderConfigurationKind::Codex,
        capabilities: ProviderCapabilities {
            provider_reported_cost: false,
            concrete_compactor: false,
        },
    },
];
