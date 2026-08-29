//! The small terminal host for [`tea_core`].
//!
//! SessionSupervisor owns terminal conversation and execution state; these
//! modules own the terminal projection and input surface. The binary is
//! intentionally small: it consumes lossless typed events, commits settled
//! rows to native scrollback, redraws only a bounded live tail through ANSI
//! sequences, and drives durable operations on Smol.
#![forbid(unsafe_code)]
#![allow(clippy::result_large_err)]

pub mod app;
pub mod build_info;
pub mod composer;
pub mod editor;
pub mod render;
pub mod terminal;
pub mod ui;

pub use app::{
    run_session_command, App, AppError, AppState, CliCommand, CliOptions, NoticeSeverity,
    SessionCommand, ToolProjection, ToolState, TranscriptEntry, UiSurface,
};
