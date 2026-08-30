//! Concrete model-provider adapters and checked-in model catalogs.
//!
//! The provider-independent ports remain in [`tea_core::scheduler`]. This crate owns concrete
//! transports, wire formats, retry behavior, and catalog data behind explicit Cargo features.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::result_large_err)]

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod error {
    pub use tea_core::error::*;
}
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod hooks {
    pub use tea_core::hooks::*;
}
mod scheduler {
    pub use tea_core::scheduler::*;
}
mod state {
    pub use tea_core::state::*;
}
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod tool {
    pub use tea_core::tool::*;
}

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod json;

mod registry;
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod retry;
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod transport_runtime;

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
pub mod openai;

pub use registry::{
    ConfiguredProvider, MODEL_CATALOG_VERSION, ModelDescriptor, ModelSelection,
    ProviderCapabilities, ProviderConfiguration, ProviderConfigurationKind, ProviderEntry,
    ProviderRegistry, RegistryError,
};
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
pub use retry::RetryPolicy;

#[cfg(feature = "provider-codex")]
pub mod codex;
#[cfg(feature = "provider-local")]
pub mod local;
#[cfg(feature = "provider-opencode-zen")]
pub mod opencode_zen;
#[cfg(feature = "provider-openrouter")]
pub mod openrouter;
