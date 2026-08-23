//! Managed sessions and externally hosted epochs built from immutable harness revisions.

#![allow(missing_docs)]

pub(crate) mod artifact;
pub(crate) mod artifact_tools;
pub(crate) mod context;
pub(crate) mod events;
pub(crate) mod harness_tool;
mod hosted;
pub(crate) mod lifecycle;
pub(crate) mod services;
pub(crate) mod session;
pub(crate) mod trace;

#[cfg(test)]
mod tests;

pub use artifact::{
    RetainedToolResult, ToolResultRetentionError, retain_tool_result_with_projection,
};
pub use context::{
    ContextAnnotation, ContextProjectionPatch, DerivedContext, ProviderLimits,
    derive_model_context, derive_model_context_with_patch,
};
pub use events::{
    ArtifactEvent, DiagnosticCode, HarnessEvent, HarnessSnapshotView, LaneSnapshotView,
    SessionEvent, TeaEvent, TeaEventSubscription, ValidationStage,
};
pub use hosted::{HostedEpoch, HostedEpochInput};
pub use services::RuntimeServices;
pub use session::{DurableOperation, HarnessIdentity, SessionRuntime};
