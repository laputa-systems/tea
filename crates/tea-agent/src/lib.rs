//! The small terminal host for [`tea_core`].
//!
//! DurableHarness owns terminal conversation and execution state; these
//! modules own the terminal projection and input surface. The binary is
//! intentionally small: it consumes lossless typed events, paints a local
//! cell grid through ANSI sequences at the rustix-backed terminal boundary,
//! and drives durable operations on Smol.
#![forbid(unsafe_code)]

pub mod app;
pub mod composer;
pub mod editor;
pub mod grid;
pub mod render;
pub mod terminal;
pub mod ui;

pub use app::{
    run_session_command, App, AppError, AppState, CliCommand, CliOptions, NoticeSeverity,
    SessionCommand, ToolProjection, ToolState, TranscriptEntry, UiSurface,
};
