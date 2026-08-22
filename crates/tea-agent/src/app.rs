//! Terminal application façade.
//!
//! The public module remains stable while the implementation is organized by
//! responsibility: command-line input, application errors, projected state,
//! host assembly, runtime/input handling, picker behavior, and presentation
//! helpers.

mod cli;
mod commands;
mod compaction;
mod error;
mod host;
mod input;
mod picker;
mod preferences;
mod runtime;
mod session;
mod state;
mod support;
mod tea;
mod workspace_ledger;

#[cfg(test)]
mod tests;

pub use cli::{CliCommand, CliError, CliOptions};
pub use compaction::ProviderCompactionStrategy;
pub use error::AppError;
pub use host::build_host_agent;
pub use runtime::App;
pub use state::{
    AppState, NoticeSeverity, ToolProjection, ToolState, TranscriptEntry, UiStatus, UiSurface,
};
pub use support::format_usage;
pub use tea::{load_tea_extensions, resolve_tea_home, TeaExtension, TeaExtensions, TeaLoadError};

/// Build a host agent with the repository-owned provider compactor configured.
///
/// This narrow constructor exists for non-interactive integration canaries. It
/// uses the same OpenAI-compatible context hook, provider, compactor prompt,
/// and automatic policy shape as the terminal host without creating UI state
/// or discovering a credential.
pub fn build_compacting_host_agent(
    tools: tea_core::DefaultCodingTools,
    model: tea_core::ModelDescriptor,
    provider: std::sync::Arc<dyn tea_core::scheduler::ModelProvider>,
    context_window: std::num::NonZeroU64,
) -> Result<tea_core::Agent, AppError> {
    build_compacting_host_agent_with_strategy(
        tools,
        model,
        provider,
        context_window,
        ProviderCompactionStrategy::default(),
    )
}

/// Build the explicit provider canary host with a non-default compaction candidate.
///
/// This constructor is intentionally separate from [`build_compacting_host_agent`]
/// so callers must opt in to any model-facing experiment by stable strategy ID.
pub fn build_compacting_host_agent_with_strategy(
    tools: tea_core::DefaultCodingTools,
    model: tea_core::ModelDescriptor,
    provider: std::sync::Arc<dyn tea_core::scheduler::ModelProvider>,
    context_window: std::num::NonZeroU64,
    strategy: ProviderCompactionStrategy,
) -> Result<tea_core::Agent, AppError> {
    let compactor = std::sync::Arc::new(compaction::ProviderCompactor::with_strategy(strategy));
    compactor.configure(model.clone(), std::sync::Arc::clone(&provider));
    let compactor_capability: std::sync::Arc<dyn tea_core::Compactor> = compactor;
    Ok(host::build_host_agent(tools)?
        .model(model)
        .model_provider(provider)
        .compactor(compactor_capability)
        .automatic_compaction(picker::automatic_compaction_policy(context_window))?
        .build())
}
