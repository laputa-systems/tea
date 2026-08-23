//! Durable supervisor for Tea core runs.
//!
//! The harness owns session-operation orchestration above the sessionless
//! `tea-core` mechanism. Its public API is intentionally introduced through
//! executable vertical slices rather than a broad placeholder surface.

mod error;
mod events;
mod capability;
mod context;
mod artifact;
mod artifact_tools;
mod harness_tool;
mod lineage;
mod lifecycle;
mod manager;
mod mode;
mod profile;
mod supervisor;
mod template;

pub use artifact::{
    retain_tool_result_with_projection, RetainedToolResult, ToolResultRetentionError,
};
pub use capability::{CapabilityBindingError, PluginCapabilityBinding, PluginCapabilityCatalog};
pub use context::{
    derive_model_context, derive_model_context_with_patch, ContextAnnotation,
    ContextProjectionPatch, DerivedContext, ProviderLimits,
};
pub use error::HarnessError;
pub use events::{
    ArtifactEvent, DiagnosticCode, HarnessEvent, HarnessSnapshotView, LaneSnapshotView,
    SessionEvent, TeaEvent, TeaEventSubscription, ValidationStage,
};
pub use lineage::{
    CandidateHypothesis, CandidateValidation, CapabilityBindingRef, HarnessActor,
    HarnessCandidateDraft, HarnessCandidateV1, HarnessLineageError, HarnessRepository,
    HarnessResourceLimits, HarnessRevisionReason, HarnessRevisionV1, HarnessSnapshotSpec,
    HarnessSnapshotV1, HarnessSurface, HarnessSurfaceFingerprints, HarnessTree,
    HarnessSourceFile, HarnessTreeFile, HarnessTreeLimits, PluginBundleRef, PromptSectionDescriptor,
    RegistryOperation, ToolPresentationDescriptor,
};
pub use manager::{
    HarnessApplyRequest, HarnessFilePatch, HarnessManager, ResolvedHarnessConfiguration,
};
pub use mode::{
    SelfExtensionMode, AUTHORING_AUTHORIZATION_METADATA_KEY, SELF_EXTENSION_MODE_METADATA_KEY,
    SELF_EXTENSION_V1_CONCISE,
};
pub use profile::{
    inspect_tool_schema_deviation, FieldMismatch, ModelHarnessProfile, ToolSchemaDeviation,
};
pub use supervisor::{DurableHarness, DurableOperation, HarnessIdentity};
pub use template::CoreEpochTemplate;

#[cfg(test)]
mod tests;
