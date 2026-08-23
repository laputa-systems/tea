//! Durable supervisor for Tea core runs.
//!
//! The harness owns session-operation orchestration above the sessionless
//! `tea-core` mechanism. Its public API is intentionally introduced through
//! executable vertical slices rather than a broad placeholder surface.

mod artifact;
mod artifact_tools;
mod capability;
mod context;
mod error;
mod events;
mod harness_tool;
mod lifecycle;
mod lineage;
mod manager;
mod mode;
mod profile;
mod supervisor;
mod template;

pub use artifact::{
    RetainedToolResult, ToolResultRetentionError, retain_tool_result_with_projection,
};
pub use capability::{CapabilityBindingError, PluginCapabilityBinding, PluginCapabilityCatalog};
pub use context::{
    ContextAnnotation, ContextProjectionPatch, DerivedContext, ProviderLimits,
    derive_model_context, derive_model_context_with_patch,
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
    HarnessSnapshotV1, HarnessSourceFile, HarnessSurface, HarnessSurfaceFingerprints, HarnessTree,
    HarnessTreeFile, HarnessTreeLimits, PluginBundleRef, PromptSectionDescriptor,
    RegistryOperation, ToolPresentationDescriptor,
};
pub use manager::{
    HarnessApplyRequest, HarnessFilePatch, HarnessManager, ResolvedHarnessConfiguration,
    verify_harness_catalog,
};
pub use mode::{
    AUTHORING_AUTHORIZATION_METADATA_KEY, SELF_EXTENSION_MODE_METADATA_KEY,
    SELF_EXTENSION_V1_CONCISE, SelfExtensionMode,
};
pub use profile::{
    FieldMismatch, ModelHarnessProfile, ToolSchemaDeviation, inspect_tool_schema_deviation,
};
pub use supervisor::{DurableHarness, DurableOperation, HarnessIdentity};
pub use template::CoreEpochTemplate;

#[cfg(test)]
mod tests;
