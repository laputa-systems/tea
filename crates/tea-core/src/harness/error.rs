//! Typed failures at the durable-supervisor boundary.

use crate::error::CoreError;
use std::fmt;

/// A failure that prevents a durable operation from continuing safely.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessError {
    /// The append-only session rejected a mutation or has faulted.
    Session(tea_session::SessionError),
    /// The session reducer found an impossible durable prefix.
    Corruption(tea_session::Corruption),
    /// The core run failed after the supervisor had recorded its durable facts.
    Core(CoreError),
    /// An immutable artifact operation could not be completed or verified.
    Artifact(tea_session::ArtifactError),
    /// A read-only durable-session verification failed.
    Verification(tea_session::SessionVerificationError),
    /// A portable durable-session export could not be completed atomically.
    Export(tea_session::SessionExportError),
    /// A caller attempted a lifecycle operation outside the harness contract.
    InvalidState { message: String },
    /// A durable prefix needs an explicit recovery decision before new work.
    RecoveryRequired { plan: tea_session::RecoveryPlan },
}

impl HarnessError {
    /// Construct a bounded lifecycle/state diagnostic.
    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::InvalidState {
            message: message.into(),
        }
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => write!(formatter, "durable session error: {error}"),
            Self::Corruption(error) => write!(formatter, "durable session corruption: {error}"),
            Self::Core(error) => write!(formatter, "core run error: {error}"),
            Self::Artifact(error) => write!(formatter, "artifact error: {error}"),
            Self::Verification(error) => {
                write!(formatter, "durable session verification failed: {error}")
            }
            Self::Export(error) => write!(formatter, "durable session export failed: {error}"),
            Self::InvalidState { message } => formatter.write_str(message),
            Self::RecoveryRequired { plan } => {
                write!(
                    formatter,
                    "durable prefix requires recovery before new work: {plan:?}"
                )
            }
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<tea_session::SessionError> for HarnessError {
    fn from(error: tea_session::SessionError) -> Self {
        match error {
            tea_session::SessionError::Corruption(corruption) => Self::Corruption(corruption),
            error => Self::Session(error),
        }
    }
}

impl From<tea_session::Corruption> for HarnessError {
    fn from(error: tea_session::Corruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<CoreError> for HarnessError {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}

impl From<tea_session::ArtifactError> for HarnessError {
    fn from(error: tea_session::ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<tea_session::SessionVerificationError> for HarnessError {
    fn from(error: tea_session::SessionVerificationError) -> Self {
        Self::Verification(error)
    }
}

impl From<tea_session::SessionExportError> for HarnessError {
    fn from(error: tea_session::SessionExportError) -> Self {
        Self::Export(error)
    }
}
