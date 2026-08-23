//! Terminal application façade.
//!
//! The terminal host is organized by command-line input, application errors,
//! projected state, durable host assembly, runtime/input handling, picker
//! behavior, and presentation helpers.

mod cli;
mod commands;
mod compaction;
mod durable;
mod error;
mod host;
mod input;
mod picker;
mod preferences;
mod runtime;
mod state;
mod support;

#[cfg(test)]
mod tests;

pub use cli::{CliCommand, CliError, CliOptions};
pub use error::AppError;
pub use runtime::App;
pub use state::{
    AppState, NoticeSeverity, ToolProjection, ToolState, TranscriptEntry, UiStatus, UiSurface,
};
pub use support::format_usage;
