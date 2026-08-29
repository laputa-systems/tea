//! Concrete model-provider adapters and checked-in model catalogs.
//!
//! The provider-independent ports remain in [`tea_core::scheduler`]. This crate owns concrete
//! transports, wire formats, retry behavior, and catalog data behind explicit Cargo features.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::result_large_err)]

mod error {
    pub use tea_core::error::*;
}
mod hooks {
    pub use tea_core::hooks::*;
}
mod scheduler {
    pub use tea_core::scheduler::*;
}
mod state {
    pub use tea_core::state::*;
}
mod tool {
    pub use tea_core::tool::*;
}

mod json;

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen"
))]
mod transport_runtime;
mod registry;
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-opencode-zen"
))]
mod retry;

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen"
))]
pub mod openai;

pub use registry::{
    ConfiguredProvider, MODEL_CATALOG_VERSION, ModelDescriptor, ModelSelection,
    ProviderCapabilities, ProviderConfiguration, ProviderConfigurationKind, ProviderEntry,
    ProviderRegistry, RegistryError,
};
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-opencode-zen"
))]
pub use retry::RetryPolicy;

#[cfg(feature = "provider-local")]
pub mod local;
#[cfg(feature = "provider-openrouter")]
pub mod openrouter;
#[cfg(feature = "provider-opencode-zen")]
pub mod opencode_zen;
