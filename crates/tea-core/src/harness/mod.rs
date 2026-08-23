//! Immutable harness lineage, resolution, and language-neutral extension contracts.

#![allow(missing_docs)]

pub mod capability;
pub mod error;
pub mod extension;
pub mod lineage;
pub mod mode;
pub mod profile;
pub mod resolver;
mod seed;

pub use capability::{CapabilityBindingError, PluginCapabilityBinding, PluginCapabilityCatalog};
pub use error::HarnessError;
pub use lineage::{
    CandidateHypothesis, CandidateValidation, CapabilityBindingRef, HarnessActor,
    HarnessCandidateDraft, HarnessCandidateV1, HarnessLineageError, HarnessRepository,
    HarnessResourceLimits, HarnessRevisionReason, HarnessRevisionV1, HarnessSnapshotSpec,
    HarnessSnapshotV1, HarnessSourceFile, HarnessSurface, HarnessSurfaceFingerprints, HarnessTree,
    HarnessTreeFile, HarnessTreeLimits, PluginBundleRef, PromptSectionDescriptor,
    RegistryOperation, ToolPresentationDescriptor,
};
pub use mode::{
    AUTHORING_AUTHORIZATION_METADATA_KEY, SELF_EXTENSION_MODE_METADATA_KEY,
    SELF_EXTENSION_V1_CONCISE, SelfExtensionMode,
};
pub use profile::{
    FieldMismatch, ModelHarnessProfile, ToolSchemaDeviation, inspect_tool_schema_deviation,
};
pub use resolver::{
    HarnessApplyRequest, HarnessFilePatch, HarnessResolver, ResolvedHarness,
    verify_harness_catalog_with_extension_engine,
};
pub use seed::{
    HarnessRuntimePolicyDescriptors, HarnessSeedBuilder, HarnessSeedExtension,
    HarnessSeedExtensionScope, SeededHarness,
};
