//! Terminal application façade.
//!
//! The terminal host is organized by command-line input, application errors,
//! projected state, durable host assembly, runtime/input handling, picker
//! behavior, and presentation helpers.

mod cli;
mod commands;
mod compaction;
mod config;
mod durable;
mod error;
mod host;
mod input;
mod mock;
mod nonblocking_operations;
mod persistence;
mod picker;
mod provider_factory;
mod runtime;
mod state;
// The pure Git engine is independently tested before later host/runtime
// wiring makes it live tool authority.
#[allow(dead_code, unused_imports)]
mod subagents;
mod support;

#[cfg(test)]
mod tests;

pub use cli::{CliCommand, CliError, CliOptions, SessionCommand};
pub use config::ConfigError;
pub use error::AppError;
pub use persistence::run_session_command;
pub use runtime::App;
pub use state::{
    AppState, NoticeSeverity, ToolProjection, ToolState, TranscriptEntry, UiStatus, UiSurface,
};
pub use support::format_usage;
