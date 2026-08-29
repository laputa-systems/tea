use crate::editor::EditorError;
use crate::terminal::TerminalError;
use std::fmt;
use tea_core::error::CoreError;
use tea_core::harness::HarnessError;
use tea_providers::RegistryError;
use tea_session::{ArtifactError, SessionError as DurableSessionError};

use crate::cli::CliError;
use super::config::ConfigError;

/// Local application failures. Provider and core failures retain their typed source.
#[derive(Debug)]
pub enum AppError {
    /// Command-line parsing failed.
    Cli(CliError),
    /// Terminal-only global configuration could not be safely loaded.
    Config(ConfigError),
    /// Terminal setup, input, output, or restoration failed.
    Terminal(TerminalError),
    /// `$EDITOR` integration failed before it could replace the composer.
    Editor(EditorError),
    /// The explicit workspace or startup selection was invalid.
    Setup(String),
    /// Registry model resolution or adapter construction failed.
    Registry(RegistryError),
    /// A core state-machine operation failed.
    Core(CoreError),
    /// The durable session/harness boundary rejected or could not safely drive an operation.
    Harness(HarnessError),
    /// The durable JSONL session could not be created or mutated before a harness owned it.
    DurableSession(DurableSessionError),
    /// A concrete immutable artifact-store operation failed at host setup.
    Artifact(ArtifactError),
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli(error) => error.fmt(formatter),
            Self::Config(error) => error.fmt(formatter),
            Self::Terminal(error) => error.fmt(formatter),
            Self::Editor(error) => error.fmt(formatter),
            Self::Setup(message) => formatter.write_str(message),
            Self::Registry(error) => error.fmt(formatter),
            Self::Core(error) => error.fmt(formatter),
            Self::Harness(error) => error.fmt(formatter),
            Self::DurableSession(error) => error.fmt(formatter),
            Self::Artifact(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cli(error) => Some(error),
            Self::Config(error) => Some(error),
            Self::Terminal(error) => Some(error),
            Self::Editor(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Core(error) => Some(error),
            Self::Harness(error) => Some(error),
            Self::DurableSession(error) => Some(error),
            Self::Artifact(error) => Some(error),
            Self::Setup(_) => None,
        }
    }
}

impl From<CliError> for AppError {
    fn from(error: CliError) -> Self {
        Self::Cli(error)
    }
}

impl From<ConfigError> for AppError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<TerminalError> for AppError {
    fn from(error: TerminalError) -> Self {
        Self::Terminal(error)
    }
}

impl From<EditorError> for AppError {
    fn from(error: EditorError) -> Self {
        Self::Editor(error)
    }
}

impl From<RegistryError> for AppError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<CoreError> for AppError {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}

impl From<HarnessError> for AppError {
    fn from(error: HarnessError) -> Self {
        Self::Harness(error)
    }
}

impl From<DurableSessionError> for AppError {
    fn from(error: DurableSessionError) -> Self {
        Self::DurableSession(error)
    }
}

impl From<ArtifactError> for AppError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}
